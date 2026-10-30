//! Serves a [`fuse3::raw::Filesystem`] to the FSKit module over a Unix socket.
//!
//! The server owns no filesystem logic: each request is translated into the
//! matching `Filesystem` call, so the FSKit mount and the macFUSE mount run
//! the same `Fuse3OrthoUnionFS` code. Requests on one connection are served
//! concurrently, one task each; replies go back through a single writer task
//! in completion order, matched to their request by id.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuse3::raw::reply::FileAttr;
use fuse3::raw::{Filesystem, Request as FuseRequest};
use futures::StreamExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use super::protocol::{
    decode_request, encode_response, inode_from_item_id, item_id_from_inode, Attr, DirEntry,
    ItemKind, Reply, Request, StatFs, Time, MAX_FRAME_LEN, READDIR_PAGE_LIMIT,
};

/// Replies queued for the writer before request tasks wait for it.
const REPLY_QUEUE_DEPTH: usize = 256;

/// Listens on a Unix socket and serves every connection the module opens.
pub struct BridgeServer {
    listener: UnixListener,
    path: PathBuf,
}

impl BridgeServer {
    /// Bind the socket at `path`, replacing any socket a previous run left.
    pub fn bind(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(path)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    /// Accept connections until `shutdown` fires, serving each against `fs`.
    ///
    /// The module normally holds one connection for the life of the mount,
    /// but it reconnects if the extension process is restarted, so the
    /// listener stays open throughout.
    pub async fn run<F>(self, fs: Arc<F>, mut shutdown: oneshot::Receiver<()>) -> io::Result<()>
    where
        F: Filesystem + Send + Sync + 'static,
    {
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    debug!(socket = %self.path.display(), "FSKit bridge: module connected");
                    let fs = Arc::clone(&fs);
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(fs, stream).await {
                            warn!(error = %e, "FSKit bridge: connection ended with an error");
                        }
                    });
                }
                _ = &mut shutdown => return Ok(()),
            }
        }
    }
}

impl Drop for BridgeServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serve one connection until the peer closes it.
pub async fn serve_connection<F, S>(fs: Arc<F>, stream: S) -> io::Result<()>
where
    F: Filesystem + Send + Sync + 'static,
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (reply_tx, mut reply_rx) = mpsc::channel::<Vec<u8>>(REPLY_QUEUE_DEPTH);

    let writer_task = tokio::spawn(async move {
        while let Some(frame) = reply_rx.recv().await {
            writer.write_all(&frame).await?;
        }
        writer.flush().await
    });

    let read_result = async {
        while let Some(body) = read_frame(&mut reader).await? {
            let (id, request) = decode_request(&body)?;
            let fs = Arc::clone(&fs);
            let reply_tx = reply_tx.clone();
            tokio::spawn(async move {
                let result = dispatch(fs.as_ref(), id, request).await;
                match encode_response(id, &result) {
                    Ok(frame) => {
                        let _ = reply_tx.send(frame).await;
                    }
                    Err(e) => warn!(id, error = %e, "FSKit bridge: reply could not be encoded"),
                }
            });
        }
        Ok::<(), io::Error>(())
    }
    .await;

    // Let in-flight requests finish and the writer drain before returning.
    drop(reply_tx);
    let write_result = writer_task
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)));
    read_result.and(write_result)
}

/// Read one frame body, or `None` on a clean end of stream.
async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the limit"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// The errno for a request naming an item that has no inode.
const BAD_ITEM: i32 = libc::ENOENT;

fn errno(err: fuse3::Errno) -> i32 {
    // fuse3 stores errnos negated for the kernel; the wire carries them positive.
    -i32::from(err)
}

fn inode(item_id: u64) -> Result<u64, i32> {
    inode_from_item_id(item_id).ok_or(BAD_ITEM)
}

async fn dispatch<F: Filesystem + Sync>(fs: &F, id: u64, request: Request) -> Result<Reply, i32> {
    let req = FuseRequest {
        unique: id,
        uid: 0,
        gid: 0,
        pid: 0,
    };
    match request {
        Request::Lookup { parent, name } => {
            let entry = fs
                .lookup(req, inode(parent)?, OsStr::from_bytes(&name))
                .await
                .map_err(errno)?;
            Ok(Reply::Attr(attr(&entry.attr, parent)))
        }
        Request::GetAttr { item_id } => {
            let reply = fs
                .getattr(req, inode(item_id)?, None, 0)
                .await
                .map_err(errno)?;
            Ok(Reply::Attr(attr(&reply.attr, 0)))
        }
        Request::ReadDir {
            item_id,
            cookie,
            with_attrs,
        } => read_dir(fs, req, item_id, cookie, with_attrs).await,
        Request::Open { item_id } => {
            let opened = fs
                .open(req, inode(item_id)?, libc::O_RDONLY as u32)
                .await
                .map_err(errno)?;
            Ok(Reply::Opened { fh: opened.fh })
        }
        Request::Read {
            item_id,
            fh,
            offset,
            length,
        } => {
            let data = fs
                .read(req, inode(item_id)?, fh, offset, length)
                .await
                .map_err(errno)?;
            Ok(Reply::Data(data.data))
        }
        Request::Release { item_id, fh } => {
            fs.release(req, inode(item_id)?, fh, libc::O_RDONLY as u32, 0, false)
                .await
                .map_err(errno)?;
            Ok(Reply::Empty)
        }
        Request::StatFs => {
            let stat = fs.statfs(req, 1).await.map_err(errno)?;
            Ok(Reply::StatFs(StatFs {
                block_size: stat.bsize,
                total_blocks: stat.blocks,
                free_blocks: stat.bfree,
                available_blocks: stat.bavail,
                total_files: stat.files,
                free_files: stat.ffree,
            }))
        }
    }
}

/// One page of a directory, starting at `cookie`.
///
/// FSKit asks for `.` and `..` only when it does not want attributes, so the
/// attribute-carrying form drops them (see `enumerateDirectory` in FSKit's
/// `FSVolume.h`). The cookie is the FUSE directory offset, which both
/// `readdir` and `readdirplus` already treat as "entries to skip".
async fn read_dir<F: Filesystem + Sync>(
    fs: &F,
    req: FuseRequest,
    item_id: u64,
    cookie: u64,
    with_attrs: bool,
) -> Result<Reply, i32> {
    let ino = inode(item_id)?;
    let mut entries = Vec::new();
    if with_attrs {
        let reply = fs
            .readdirplus(req, ino, 0, cookie, 0)
            .await
            .map_err(errno)?;
        let mut stream = std::pin::pin!(reply.entries);
        while let Some(entry) = stream.next().await {
            let entry = entry.map_err(errno)?;
            if is_dot(entry.name.as_bytes()) {
                continue;
            }
            entries.push(DirEntry {
                item_id: item_id_from_inode(entry.inode),
                kind: entry.kind.into(),
                next_cookie: entry.offset as u64,
                name: entry.name.as_bytes().to_vec(),
                attr: Some(attr(&entry.attr, item_id)),
            });
            if entries.len() == READDIR_PAGE_LIMIT {
                break;
            }
        }
    } else {
        let reply = fs
            .readdir(req, ino, 0, cookie as i64)
            .await
            .map_err(errno)?;
        let mut stream = std::pin::pin!(reply.entries);
        while let Some(entry) = stream.next().await {
            let entry = entry.map_err(errno)?;
            entries.push(DirEntry {
                item_id: item_id_from_inode(entry.inode),
                kind: entry.kind.into(),
                next_cookie: entry.offset as u64,
                name: entry.name.as_bytes().to_vec(),
                attr: None,
            });
            if entries.len() == READDIR_PAGE_LIMIT {
                break;
            }
        }
    }
    Ok(Reply::Dir(entries))
}

// POSIX file type bits. libc's `mode_t` is u16 on macOS and u32 on Linux, so
// spelling them out keeps the wire's u16 mode identical on both.
const S_IFDIR: u16 = 0o040000;
const S_IFLNK: u16 = 0o120000;
const S_IFREG: u16 = 0o100000;

fn is_dot(name: &[u8]) -> bool {
    name == b"." || name == b".."
}

/// Translate a FUSE attribute to the wire form.
///
/// `parent_id` is 0 when the caller does not know the parent, as for a bare
/// `getattr`; the module keeps the parent it learnt at lookup.
fn attr(fuse: &FileAttr, parent_id: u64) -> Attr {
    let kind = ItemKind::from(fuse.kind);
    let type_bits = match kind {
        ItemKind::Directory => S_IFDIR,
        ItemKind::Symlink => S_IFLNK,
        _ => S_IFREG,
    };
    let time = |t: fuse3::Timestamp| Time {
        sec: t.sec,
        nsec: t.nsec,
    };
    Attr {
        item_id: item_id_from_inode(fuse.ino),
        parent_id,
        size: fuse.size,
        alloc_size: fuse.blocks * 512,
        kind,
        mode: type_bits | (fuse.perm & 0o7777),
        nlink: fuse.nlink,
        uid: fuse.uid,
        gid: fuse.gid,
        atime: time(fuse.atime),
        mtime: time(fuse.mtime),
        ctime: time(fuse.ctime),
        #[cfg(target_os = "macos")]
        birthtime: time(fuse.crtime),
        // Linux FUSE has no creation time; the bridge only serves macOS, but
        // builds everywhere so its tests run in CI.
        #[cfg(not(target_os = "macos"))]
        birthtime: time(fuse.ctime),
    }
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{decode_response, encode_request, ROOT_ITEM_ID};
    use super::*;
    use bytes::Bytes;
    use fuse3::raw::reply::{
        DirectoryEntry, DirectoryEntryPlus, ReplyAttr, ReplyData, ReplyDirectory,
        ReplyDirectoryPlus, ReplyEntry, ReplyInit, ReplyOpen, ReplyStatFs,
    };
    use fuse3::{FileType, Inode, Result as FuseResult, Timestamp};
    use futures::stream::{self, Stream};
    use std::ffi::OsString;
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::net::UnixStream;

    const FILE_INO: u64 = 2;
    const CONTENT: &[u8] = b"0123456789";

    /// A root holding one ten-byte file, `a.dds`, and a count of releases.
    #[derive(Default)]
    struct OneFileFs {
        released: AtomicU64,
    }

    fn file_attr(ino: u64) -> FileAttr {
        let kind = if ino == 1 {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let t = Timestamp::new(1_700_000_000, 5);
        FileAttr {
            ino,
            size: if ino == 1 { 0 } else { CONTENT.len() as u64 },
            blocks: 1,
            atime: t,
            mtime: t,
            ctime: t,
            #[cfg(target_os = "macos")]
            crtime: t,
            kind,
            perm: if ino == 1 { 0o755 } else { 0o444 },
            nlink: 1,
            uid: 501,
            gid: 20,
            rdev: 0,
            #[cfg(target_os = "macos")]
            flags: 0,
            blksize: 4096,
        }
    }

    impl Filesystem for OneFileFs {
        async fn init(&self, _req: FuseRequest) -> FuseResult<ReplyInit> {
            Ok(ReplyInit::new(NonZeroU32::new(1 << 20).unwrap()))
        }

        async fn destroy(&self, _req: FuseRequest) {}

        async fn lookup(
            &self,
            _req: FuseRequest,
            parent: Inode,
            name: &OsStr,
        ) -> FuseResult<ReplyEntry> {
            if parent == 1 && name == "a.dds" {
                Ok(ReplyEntry {
                    ttl: Duration::from_secs(1),
                    attr: file_attr(FILE_INO),
                    generation: 0,
                })
            } else {
                Err(libc::ENOENT.into())
            }
        }

        async fn getattr(
            &self,
            _req: FuseRequest,
            inode: Inode,
            _fh: Option<u64>,
            _flags: u32,
        ) -> FuseResult<ReplyAttr> {
            Ok(ReplyAttr {
                ttl: Duration::from_secs(1),
                attr: file_attr(inode),
            })
        }

        async fn open(
            &self,
            _req: FuseRequest,
            _inode: Inode,
            _flags: u32,
        ) -> FuseResult<ReplyOpen> {
            Ok(ReplyOpen { fh: 77, flags: 0 })
        }

        async fn read(
            &self,
            _req: FuseRequest,
            _inode: Inode,
            fh: u64,
            offset: u64,
            size: u32,
        ) -> FuseResult<ReplyData> {
            assert_eq!(fh, 77, "read must use the handle open returned");
            let start = (offset as usize).min(CONTENT.len());
            let end = (start + size as usize).min(CONTENT.len());
            Ok(ReplyData {
                data: Bytes::copy_from_slice(&CONTENT[start..end]),
            })
        }

        async fn release(
            &self,
            _req: FuseRequest,
            _inode: Inode,
            _fh: u64,
            _flags: u32,
            _lock_owner: u64,
            _flush: bool,
        ) -> FuseResult<()> {
            self.released.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn readdir<'a>(
            &'a self,
            _req: FuseRequest,
            _parent: Inode,
            _fh: u64,
            offset: i64,
        ) -> FuseResult<ReplyDirectory<impl Stream<Item = FuseResult<DirectoryEntry>> + Send + 'a>>
        {
            let entries = [(1, ".", 1), (1, "..", 2), (FILE_INO, "a.dds", 3)]
                .into_iter()
                .skip(offset as usize)
                .map(|(inode, name, offset)| {
                    Ok(DirectoryEntry {
                        inode,
                        kind: if inode == 1 {
                            FileType::Directory
                        } else {
                            FileType::RegularFile
                        },
                        name: OsString::from(name),
                        offset,
                    })
                })
                .collect::<Vec<_>>();
            Ok(ReplyDirectory {
                entries: stream::iter(entries),
            })
        }

        async fn readdirplus<'a>(
            &'a self,
            _req: FuseRequest,
            _parent: Inode,
            _fh: u64,
            offset: u64,
            _lock_owner: u64,
        ) -> FuseResult<
            ReplyDirectoryPlus<impl Stream<Item = FuseResult<DirectoryEntryPlus>> + Send + 'a>,
        > {
            let entries = [(1, ".", 1), (1, "..", 2), (FILE_INO, "a.dds", 3)]
                .into_iter()
                .skip(offset as usize)
                .map(|(inode, name, offset)| {
                    let attr = file_attr(inode);
                    Ok(DirectoryEntryPlus {
                        inode,
                        generation: 0,
                        kind: attr.kind,
                        name: OsString::from(name),
                        offset,
                        attr,
                        entry_ttl: Duration::from_secs(1),
                        attr_ttl: Duration::from_secs(1),
                    })
                })
                .collect::<Vec<_>>();
            Ok(ReplyDirectoryPlus {
                entries: stream::iter(entries),
            })
        }

        async fn statfs(&self, _req: FuseRequest, _inode: Inode) -> FuseResult<ReplyStatFs> {
            Ok(ReplyStatFs {
                blocks: 10,
                bfree: 0,
                bavail: 0,
                files: 1,
                ffree: 0,
                bsize: 4096,
                namelen: 255,
                frsize: 4096,
            })
        }
    }

    /// A client end of a connection served against a fresh [`OneFileFs`].
    struct Client {
        stream: UnixStream,
        fs: Arc<OneFileFs>,
        next_id: u64,
    }

    impl Client {
        fn new() -> Self {
            let (client, server) = UnixStream::pair().unwrap();
            let fs = Arc::new(OneFileFs::default());
            tokio::spawn(serve_connection(Arc::clone(&fs), server));
            Self {
                stream: client,
                fs,
                next_id: 1,
            }
        }

        async fn call(&mut self, request: Request) -> Result<Reply, i32> {
            let id = self.next_id;
            self.next_id += 1;
            self.stream
                .write_all(&encode_request(id, &request).unwrap())
                .await
                .unwrap();
            let body = read_frame(&mut self.stream).await.unwrap().unwrap();
            let (reply_id, result) = decode_response(&body, &request).unwrap();
            assert_eq!(reply_id, id);
            result
        }
    }

    #[tokio::test]
    async fn lookup_returns_fskit_ids_and_the_parent() {
        let mut client = Client::new();
        let Ok(Reply::Attr(attr)) = client
            .call(Request::Lookup {
                parent: ROOT_ITEM_ID,
                name: b"a.dds".to_vec(),
            })
            .await
        else {
            panic!("lookup failed");
        };
        assert_eq!(attr.item_id, FILE_INO + 1);
        assert_eq!(attr.parent_id, ROOT_ITEM_ID);
        assert_eq!(attr.kind, ItemKind::File);
        assert_eq!(attr.size, CONTENT.len() as u64);
        assert_eq!(attr.mode, S_IFREG | 0o444);
    }

    #[tokio::test]
    async fn a_missing_name_is_enoent() {
        let mut client = Client::new();
        let result = client
            .call(Request::Lookup {
                parent: ROOT_ITEM_ID,
                name: b"nope".to_vec(),
            })
            .await;
        assert_eq!(result, Err(libc::ENOENT));
    }

    #[tokio::test]
    async fn the_root_parent_id_has_no_inode() {
        let mut client = Client::new();
        assert_eq!(
            client.call(Request::GetAttr { item_id: 1 }).await,
            Err(libc::ENOENT)
        );
    }

    #[tokio::test]
    async fn root_getattr_is_a_directory() {
        let mut client = Client::new();
        let Ok(Reply::Attr(attr)) = client
            .call(Request::GetAttr {
                item_id: ROOT_ITEM_ID,
            })
            .await
        else {
            panic!("getattr failed");
        };
        assert_eq!(attr.item_id, ROOT_ITEM_ID);
        assert_eq!(attr.kind, ItemKind::Directory);
        assert_eq!(attr.mode, S_IFDIR | 0o755);
    }

    #[tokio::test]
    async fn readdir_without_attrs_includes_dot_entries() {
        let mut client = Client::new();
        let Ok(Reply::Dir(entries)) = client
            .call(Request::ReadDir {
                item_id: ROOT_ITEM_ID,
                cookie: 0,
                with_attrs: false,
            })
            .await
        else {
            panic!("readdir failed");
        };
        let names: Vec<_> = entries.iter().map(|e| e.name.as_slice()).collect();
        assert_eq!(names, [&b"."[..], b"..", b"a.dds"]);
        assert_eq!(entries[0].item_id, ROOT_ITEM_ID);
        assert!(entries.iter().all(|e| e.attr.is_none()));
    }

    #[tokio::test]
    async fn readdir_with_attrs_drops_dot_entries() {
        let mut client = Client::new();
        let Ok(Reply::Dir(entries)) = client
            .call(Request::ReadDir {
                item_id: ROOT_ITEM_ID,
                cookie: 0,
                with_attrs: true,
            })
            .await
        else {
            panic!("readdir failed");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, b"a.dds");
        assert_eq!(entries[0].next_cookie, 3);
        let attr = entries[0].attr.as_ref().unwrap();
        assert_eq!(attr.parent_id, ROOT_ITEM_ID);
    }

    #[tokio::test]
    async fn readdir_resumes_from_the_cookie() {
        let mut client = Client::new();
        let Ok(Reply::Dir(entries)) = client
            .call(Request::ReadDir {
                item_id: ROOT_ITEM_ID,
                cookie: 2,
                with_attrs: false,
            })
            .await
        else {
            panic!("readdir failed");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, b"a.dds");
    }

    #[tokio::test]
    async fn open_read_release_uses_the_handle() {
        let mut client = Client::new();
        let item_id = FILE_INO + 1;
        let Ok(Reply::Opened { fh }) = client.call(Request::Open { item_id }).await else {
            panic!("open failed");
        };
        let data = client
            .call(Request::Read {
                item_id,
                fh,
                offset: 3,
                length: 4,
            })
            .await;
        assert_eq!(data, Ok(Reply::Data(Bytes::from_static(b"3456"))));
        assert_eq!(
            client.call(Request::Release { item_id, fh }).await,
            Ok(Reply::Empty)
        );
        assert_eq!(client.fs.released.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn statfs_maps_fields() {
        let mut client = Client::new();
        let Ok(Reply::StatFs(stat)) = client.call(Request::StatFs).await else {
            panic!("statfs failed");
        };
        assert_eq!(stat.block_size, 4096);
        assert_eq!(stat.total_blocks, 10);
        assert_eq!(stat.total_files, 1);
    }

    #[tokio::test]
    async fn requests_in_flight_together_are_all_answered() {
        let (mut client, server) = UnixStream::pair().unwrap();
        tokio::spawn(serve_connection(Arc::new(OneFileFs::default()), server));
        let request = Request::GetAttr {
            item_id: ROOT_ITEM_ID,
        };
        for id in 1..=32 {
            client
                .write_all(&encode_request(id, &request).unwrap())
                .await
                .unwrap();
        }
        let mut seen = Vec::new();
        for _ in 1..=32 {
            let body = read_frame(&mut client).await.unwrap().unwrap();
            let (id, result) = decode_response(&body, &request).unwrap();
            assert!(result.is_ok());
            seen.push(id);
        }
        seen.sort_unstable();
        assert_eq!(seen, (1..=32).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn an_oversized_frame_ends_the_connection() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let served = tokio::spawn(serve_connection(Arc::new(OneFileFs::default()), server));
        client
            .write_all(&(MAX_FRAME_LEN + 1).to_le_bytes())
            .await
            .unwrap();
        let err = served.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn the_server_accepts_on_its_socket_and_removes_it_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.sock");
        let server = BridgeServer::bind(&path).unwrap();
        let (stop_tx, stop_rx) = oneshot::channel();
        let running = tokio::spawn(server.run(Arc::new(OneFileFs::default()), stop_rx));

        let stream = UnixStream::connect(&path).await.unwrap();
        let mut client = Client {
            stream,
            fs: Arc::new(OneFileFs::default()),
            next_id: 1,
        };
        assert!(client.call(Request::StatFs).await.is_ok());

        stop_tx.send(()).unwrap();
        running.await.unwrap().unwrap();
        assert!(
            !path.exists(),
            "socket must be removed when the server stops"
        );
    }

    #[tokio::test]
    async fn bind_replaces_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.sock");
        let first = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(first); // leaves the socket file behind, as a crash would
        assert!(path.exists());
        assert!(BridgeServer::bind(&path).is_ok());
    }
}
