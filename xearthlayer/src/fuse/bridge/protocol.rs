//! Wire format spoken between the FSKit module and the bridge server.
//!
//! Every message is a frame: a little-endian `u32` body length followed by the
//! body. Request bodies start with a `u64` request id and a `u8` opcode;
//! response bodies start with the same id and an `i32` errno, zero on success,
//! followed by the opcode's payload only when the errno is zero. Ids let the
//! module keep many requests in flight on one connection, which matters
//! because X-Plane reads textures in parallel.
//!
//! Item identifiers on the wire are FSKit's, not FUSE's: FSKit reserves 1 for
//! the parent of the root and 2 for the root, where FUSE numbers the root 1.
//! The server shifts by one in each direction (see [`item_id_from_inode`]), so
//! the Swift side never has to know FUSE inode numbering.
//!
//! The Swift codec in `macos/fskit/Sources/BridgeProtocol.swift` mirrors this
//! file; a change to either must be made to both.

use std::io;

/// Frames larger than this are rejected rather than allocated. A read reply
/// carries at most one FSKit read (a few MiB), so 64 MiB is far above any
/// legitimate frame while still bounding a corrupt length prefix.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Most directory entries returned by one `ReadDir`. FSKit pulls a directory
/// in pages anyway; capping the page bounds the frame for a terrain directory
/// with tens of thousands of entries.
pub const READDIR_PAGE_LIMIT: usize = 512;

/// FSKit item id of the root directory (`FSItemIDRootDirectory`).
pub const ROOT_ITEM_ID: u64 = 2;

/// Convert a FUSE inode to an FSKit item id.
pub fn item_id_from_inode(inode: u64) -> u64 {
    inode + 1
}

/// Convert an FSKit item id to a FUSE inode.
///
/// Ids below the root (0 is invalid, 1 is the root's parent) have no inode.
pub fn inode_from_item_id(item_id: u64) -> Option<u64> {
    (item_id >= ROOT_ITEM_ID).then(|| item_id - 1)
}

/// The kind of a filesystem item, as FSKit's `FSItemType` numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ItemKind {
    Unknown = 0,
    File = 1,
    Directory = 2,
    Symlink = 3,
}

impl ItemKind {
    fn from_u8(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Unknown),
            1 => Ok(Self::File),
            2 => Ok(Self::Directory),
            3 => Ok(Self::Symlink),
            other => Err(invalid(format!("unknown item kind {other}"))),
        }
    }
}

impl From<fuse3::FileType> for ItemKind {
    fn from(kind: fuse3::FileType) -> Self {
        match kind {
            fuse3::FileType::RegularFile => Self::File,
            fuse3::FileType::Directory => Self::Directory,
            fuse3::FileType::Symlink => Self::Symlink,
            _ => Self::Unknown,
        }
    }
}

/// Seconds and nanoseconds since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

/// The attributes of one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub item_id: u64,
    pub parent_id: u64,
    pub size: u64,
    pub alloc_size: u64,
    pub kind: ItemKind,
    pub mode: u16,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
    pub birthtime: Time,
}

/// One directory entry, with the cookie that resumes enumeration after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub item_id: u64,
    pub kind: ItemKind,
    pub next_cookie: u64,
    pub name: Vec<u8>,
    pub attr: Option<Attr>,
}

/// Volume statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatFs {
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub available_blocks: u64,
    pub total_files: u64,
    pub free_files: u64,
}

/// An operation the FSKit module asks the server to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Find `name` in directory `parent`.
    Lookup { parent: u64, name: Vec<u8> },
    /// Attributes of an item.
    GetAttr { item_id: u64 },
    /// One page of a directory, from `cookie` (0 for the start).
    ReadDir {
        item_id: u64,
        cookie: u64,
        with_attrs: bool,
    },
    /// Open a file for reading; the reply carries the handle reads use.
    Open { item_id: u64 },
    /// Read up to `length` bytes at `offset` through handle `fh`.
    Read {
        item_id: u64,
        fh: u64,
        offset: u64,
        length: u32,
    },
    /// Release a handle returned by `Open`.
    Release { item_id: u64, fh: u64 },
    /// Volume statistics.
    StatFs,
}

/// The payload of a successful reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Attr(Attr),
    Dir(Vec<DirEntry>),
    Opened { fh: u64 },
    Data(bytes::Bytes),
    Empty,
    StatFs(StatFs),
}

const OP_LOOKUP: u8 = 1;
const OP_GETATTR: u8 = 2;
const OP_READDIR: u8 = 3;
const OP_OPEN: u8 = 4;
const OP_READ: u8 = 5;
const OP_RELEASE: u8 = 6;
const OP_STATFS: u8 = 7;

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Sequential reader over a frame body.
struct Cursor<'a> {
    buf: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.buf.len() < n {
            return Err(invalid(format!(
                "frame truncated: wanted {n} bytes, {} left",
                self.buf.len()
            )));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> io::Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn name(&mut self) -> io::Result<Vec<u8>> {
        let len = self.u16()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn time(&mut self) -> io::Result<Time> {
        Ok(Time {
            sec: self.i64()?,
            nsec: self.u32()?,
        })
    }

    fn attr(&mut self) -> io::Result<Attr> {
        Ok(Attr {
            item_id: self.u64()?,
            parent_id: self.u64()?,
            size: self.u64()?,
            alloc_size: self.u64()?,
            kind: ItemKind::from_u8(self.u8()?)?,
            mode: self.u16()?,
            nlink: self.u32()?,
            uid: self.u32()?,
            gid: self.u32()?,
            atime: self.time()?,
            mtime: self.time()?,
            ctime: self.time()?,
            birthtime: self.time()?,
        })
    }

    fn finish(self) -> io::Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(invalid(format!(
                "{} trailing bytes in frame",
                self.buf.len()
            )))
        }
    }
}

fn put_name(out: &mut Vec<u8>, name: &[u8]) -> io::Result<()> {
    let len = u16::try_from(name.len()).map_err(|_| invalid("name too long".into()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(name);
    Ok(())
}

fn put_time(out: &mut Vec<u8>, time: Time) {
    out.extend_from_slice(&time.sec.to_le_bytes());
    out.extend_from_slice(&time.nsec.to_le_bytes());
}

fn put_attr(out: &mut Vec<u8>, attr: &Attr) {
    out.extend_from_slice(&attr.item_id.to_le_bytes());
    out.extend_from_slice(&attr.parent_id.to_le_bytes());
    out.extend_from_slice(&attr.size.to_le_bytes());
    out.extend_from_slice(&attr.alloc_size.to_le_bytes());
    out.push(attr.kind as u8);
    out.extend_from_slice(&attr.mode.to_le_bytes());
    out.extend_from_slice(&attr.nlink.to_le_bytes());
    out.extend_from_slice(&attr.uid.to_le_bytes());
    out.extend_from_slice(&attr.gid.to_le_bytes());
    put_time(out, attr.atime);
    put_time(out, attr.mtime);
    put_time(out, attr.ctime);
    put_time(out, attr.birthtime);
}

/// Prefix `body` with its length, producing a complete frame.
fn frame(body: Vec<u8>) -> io::Result<Vec<u8>> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME_LEN)
        .ok_or_else(|| invalid(format!("frame of {} bytes too large", body.len())))?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Encode a request as a complete frame.
pub fn encode_request(id: u64, request: &Request) -> io::Result<Vec<u8>> {
    let mut body = id.to_le_bytes().to_vec();
    match request {
        Request::Lookup { parent, name } => {
            body.push(OP_LOOKUP);
            body.extend_from_slice(&parent.to_le_bytes());
            put_name(&mut body, name)?;
        }
        Request::GetAttr { item_id } => {
            body.push(OP_GETATTR);
            body.extend_from_slice(&item_id.to_le_bytes());
        }
        Request::ReadDir {
            item_id,
            cookie,
            with_attrs,
        } => {
            body.push(OP_READDIR);
            body.extend_from_slice(&item_id.to_le_bytes());
            body.extend_from_slice(&cookie.to_le_bytes());
            body.push(u8::from(*with_attrs));
        }
        Request::Open { item_id } => {
            body.push(OP_OPEN);
            body.extend_from_slice(&item_id.to_le_bytes());
        }
        Request::Read {
            item_id,
            fh,
            offset,
            length,
        } => {
            body.push(OP_READ);
            body.extend_from_slice(&item_id.to_le_bytes());
            body.extend_from_slice(&fh.to_le_bytes());
            body.extend_from_slice(&offset.to_le_bytes());
            body.extend_from_slice(&length.to_le_bytes());
        }
        Request::Release { item_id, fh } => {
            body.push(OP_RELEASE);
            body.extend_from_slice(&item_id.to_le_bytes());
            body.extend_from_slice(&fh.to_le_bytes());
        }
        Request::StatFs => body.push(OP_STATFS),
    }
    frame(body)
}

/// Decode a request body (the frame without its length prefix).
pub fn decode_request(body: &[u8]) -> io::Result<(u64, Request)> {
    let mut cur = Cursor { buf: body };
    let id = cur.u64()?;
    let request = match cur.u8()? {
        OP_LOOKUP => Request::Lookup {
            parent: cur.u64()?,
            name: cur.name()?,
        },
        OP_GETATTR => Request::GetAttr {
            item_id: cur.u64()?,
        },
        OP_READDIR => Request::ReadDir {
            item_id: cur.u64()?,
            cookie: cur.u64()?,
            with_attrs: cur.u8()? != 0,
        },
        OP_OPEN => Request::Open {
            item_id: cur.u64()?,
        },
        OP_READ => Request::Read {
            item_id: cur.u64()?,
            fh: cur.u64()?,
            offset: cur.u64()?,
            length: cur.u32()?,
        },
        OP_RELEASE => Request::Release {
            item_id: cur.u64()?,
            fh: cur.u64()?,
        },
        OP_STATFS => Request::StatFs,
        other => return Err(invalid(format!("unknown opcode {other}"))),
    };
    cur.finish()?;
    Ok((id, request))
}

/// Encode a reply, or an errno when `result` is an error, as a complete frame.
pub fn encode_response(id: u64, result: &Result<Reply, i32>) -> io::Result<Vec<u8>> {
    let mut body = id.to_le_bytes().to_vec();
    match result {
        Err(errno) => body.extend_from_slice(&errno.to_le_bytes()),
        Ok(reply) => {
            body.extend_from_slice(&0i32.to_le_bytes());
            match reply {
                Reply::Attr(attr) => put_attr(&mut body, attr),
                Reply::Dir(entries) => {
                    body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
                    for entry in entries {
                        body.extend_from_slice(&entry.item_id.to_le_bytes());
                        body.push(entry.kind as u8);
                        body.extend_from_slice(&entry.next_cookie.to_le_bytes());
                        put_name(&mut body, &entry.name)?;
                        match &entry.attr {
                            Some(attr) => {
                                body.push(1);
                                put_attr(&mut body, attr);
                            }
                            None => body.push(0),
                        }
                    }
                }
                Reply::Opened { fh } => body.extend_from_slice(&fh.to_le_bytes()),
                Reply::Data(data) => body.extend_from_slice(data),
                Reply::Empty => {}
                Reply::StatFs(stat) => {
                    body.extend_from_slice(&stat.block_size.to_le_bytes());
                    body.extend_from_slice(&stat.total_blocks.to_le_bytes());
                    body.extend_from_slice(&stat.free_blocks.to_le_bytes());
                    body.extend_from_slice(&stat.available_blocks.to_le_bytes());
                    body.extend_from_slice(&stat.total_files.to_le_bytes());
                    body.extend_from_slice(&stat.free_files.to_le_bytes());
                }
            }
        }
    }
    frame(body)
}

/// Decode a response body for a request of the given kind.
///
/// The reply layout depends on the request, which the wire does not repeat,
/// so the caller says what it asked. Used by tests and by any Rust client.
pub fn decode_response(body: &[u8], request: &Request) -> io::Result<(u64, Result<Reply, i32>)> {
    let mut cur = Cursor { buf: body };
    let id = cur.u64()?;
    let errno = cur.i32()?;
    if errno != 0 {
        cur.finish()?;
        return Ok((id, Err(errno)));
    }
    let reply = match request {
        Request::Lookup { .. } | Request::GetAttr { .. } => Reply::Attr(cur.attr()?),
        Request::ReadDir { .. } => {
            let count = cur.u32()?;
            let mut entries = Vec::with_capacity(count.min(READDIR_PAGE_LIMIT as u32) as usize);
            for _ in 0..count {
                entries.push(DirEntry {
                    item_id: cur.u64()?,
                    kind: ItemKind::from_u8(cur.u8()?)?,
                    next_cookie: cur.u64()?,
                    name: cur.name()?,
                    attr: if cur.u8()? != 0 {
                        Some(cur.attr()?)
                    } else {
                        None
                    },
                });
            }
            Reply::Dir(entries)
        }
        Request::Open { .. } => Reply::Opened { fh: cur.u64()? },
        Request::Read { .. } => {
            let data = bytes::Bytes::copy_from_slice(cur.buf);
            cur.buf = &[];
            Reply::Data(data)
        }
        Request::Release { .. } => Reply::Empty,
        Request::StatFs => Reply::StatFs(StatFs {
            block_size: cur.u32()?,
            total_blocks: cur.u64()?,
            free_blocks: cur.u64()?,
            available_blocks: cur.u64()?,
            total_files: cur.u64()?,
            free_files: cur.u64()?,
        }),
    };
    cur.finish()?;
    Ok((id, Ok(reply)))
}

/// Split the length prefix off a frame, for tests that hold whole frames.
#[cfg(test)]
fn body(frame: &[u8]) -> &[u8] {
    let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
    assert_eq!(frame.len(), 4 + len, "length prefix disagrees with frame");
    &frame[4..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_attr() -> Attr {
        Attr {
            item_id: 42,
            parent_id: ROOT_ITEM_ID,
            size: 11_184_952,
            alloc_size: 11_186_176,
            kind: ItemKind::File,
            mode: 0o100444,
            nlink: 1,
            uid: 501,
            gid: 20,
            atime: Time { sec: 1, nsec: 2 },
            mtime: Time { sec: 3, nsec: 4 },
            ctime: Time { sec: 5, nsec: 6 },
            birthtime: Time { sec: -7, nsec: 8 },
        }
    }

    fn all_requests() -> Vec<Request> {
        vec![
            Request::Lookup {
                parent: ROOT_ITEM_ID,
                name: b"terrain".to_vec(),
            },
            Request::GetAttr { item_id: 9 },
            Request::ReadDir {
                item_id: ROOT_ITEM_ID,
                cookie: 17,
                with_attrs: true,
            },
            Request::Open { item_id: 9 },
            Request::Read {
                item_id: 9,
                fh: 3,
                offset: 1 << 33,
                length: 1 << 20,
            },
            Request::Release { item_id: 9, fh: 3 },
            Request::StatFs,
        ]
    }

    #[test]
    fn every_request_round_trips() {
        for (id, request) in (1u64..).zip(all_requests()) {
            let frame = encode_request(id, &request).unwrap();
            assert_eq!(decode_request(body(&frame)).unwrap(), (id, request));
        }
    }

    #[test]
    fn lookup_encodes_to_the_documented_bytes() {
        // Pins the layout the Swift codec mirrors: a change here is a protocol
        // change and must be made in BridgeProtocol.swift too.
        let frame = encode_request(
            0x0102,
            &Request::Lookup {
                parent: 2,
                name: b"ab".to_vec(),
            },
        )
        .unwrap();
        assert_eq!(
            frame,
            [
                21, 0, 0, 0, // body length
                0x02, 0x01, 0, 0, 0, 0, 0, 0, // id
                1, // opcode
                2, 0, 0, 0, 0, 0, 0, 0, // parent
                2, 0, b'a', b'b', // name
            ]
        );
    }

    #[test]
    fn every_reply_round_trips() {
        let cases = vec![
            (Request::GetAttr { item_id: 42 }, Reply::Attr(sample_attr())),
            (
                Request::ReadDir {
                    item_id: ROOT_ITEM_ID,
                    cookie: 0,
                    with_attrs: true,
                },
                Reply::Dir(vec![
                    DirEntry {
                        item_id: 42,
                        kind: ItemKind::File,
                        next_cookie: 1,
                        name: b"a.dds".to_vec(),
                        attr: Some(sample_attr()),
                    },
                    DirEntry {
                        item_id: 43,
                        kind: ItemKind::Directory,
                        next_cookie: 2,
                        name: b"textures".to_vec(),
                        attr: None,
                    },
                ]),
            ),
            (Request::Open { item_id: 42 }, Reply::Opened { fh: 7 }),
            (
                Request::Read {
                    item_id: 42,
                    fh: 7,
                    offset: 0,
                    length: 4,
                },
                Reply::Data(bytes::Bytes::from_static(b"DDS ")),
            ),
            (Request::Release { item_id: 42, fh: 7 }, Reply::Empty),
            (
                Request::StatFs,
                Reply::StatFs(StatFs {
                    block_size: 4096,
                    total_blocks: 1_000_000,
                    free_blocks: 0,
                    available_blocks: 0,
                    total_files: 12,
                    free_files: 0,
                }),
            ),
        ];
        for (id, (request, reply)) in (1u64..).zip(cases) {
            let frame = encode_response(id, &Ok(reply.clone())).unwrap();
            assert_eq!(
                decode_response(body(&frame), &request).unwrap(),
                (id, Ok(reply))
            );
        }
    }

    #[test]
    fn an_error_reply_carries_only_the_errno() {
        let frame = encode_response(5, &Err(libc::ENOENT)).unwrap();
        assert_eq!(body(&frame).len(), 12);
        assert_eq!(
            decode_response(body(&frame), &Request::GetAttr { item_id: 1 }).unwrap(),
            (5, Err(libc::ENOENT))
        );
    }

    #[test]
    fn a_truncated_request_is_rejected() {
        let frame = encode_request(1, &Request::Open { item_id: 9 }).unwrap();
        let body = body(&frame);
        let err = decode_request(&body[..body.len() - 1]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let frame = encode_request(1, &Request::StatFs).unwrap();
        let mut body = body(&frame).to_vec();
        body.push(0);
        assert!(decode_request(&body).is_err());
    }

    #[test]
    fn an_unknown_opcode_is_rejected() {
        let mut body = 1u64.to_le_bytes().to_vec();
        body.push(200);
        assert!(decode_request(&body).is_err());
    }

    #[test]
    fn a_name_longer_than_u16_cannot_be_encoded() {
        let request = Request::Lookup {
            parent: ROOT_ITEM_ID,
            name: vec![b'x'; 70_000],
        };
        assert!(encode_request(1, &request).is_err());
    }

    #[test]
    fn item_ids_are_inodes_shifted_by_one() {
        assert_eq!(item_id_from_inode(1), ROOT_ITEM_ID);
        assert_eq!(inode_from_item_id(ROOT_ITEM_ID), Some(1));
        assert_eq!(inode_from_item_id(1), None);
        assert_eq!(inode_from_item_id(0), None);
        let virtual_inode = 0x1000_0000_0000_0000 + (5 << 32);
        assert_eq!(
            inode_from_item_id(item_id_from_inode(virtual_inode)),
            Some(virtual_inode)
        );
    }
}
