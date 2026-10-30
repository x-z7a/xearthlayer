//! The FSKit bridge serving the production `Fuse3OrthoUnionFS` (#210).
//!
//! Plays the FSKit module's part over a socket pair: lookup → getattr →
//! readdir → open → read → release against a real ortho union index, with no
//! kernel involved. This is what the Swift module asks for when X-Plane reads
//! through an FSKit mount, so it runs everywhere, including Linux CI.
//!
//! The live mount, through the module, is `fskit_smoke.rs`.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use xearthlayer::executor::ChannelDdsClient;
use xearthlayer::fuse::bridge::protocol::{
    decode_response, encode_request, ItemKind, Reply, Request, ROOT_ITEM_ID,
};
use xearthlayer::fuse::bridge::serve_connection;
use xearthlayer::fuse::fuse3::Fuse3OrthoUnionFS;
use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
use xearthlayer::runtime::JobRequest;

const DSF_CONTENTS: &[u8] = b"fake dsf payload for the fskit bridge";

struct Module {
    stream: UnixStream,
    next_id: u64,
}

impl Module {
    async fn call(&mut self, request: Request) -> Result<Reply, i32> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = encode_request(id, &request).unwrap();
        self.stream.write_all(&frame).await.unwrap();

        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        self.stream.read_exact(&mut body).await.unwrap();
        let (reply_id, result) = decode_response(&body, &request).unwrap();
        assert_eq!(reply_id, id);
        result
    }

    async fn lookup(&mut self, parent: u64, name: &str) -> u64 {
        match self
            .call(Request::Lookup {
                parent,
                name: name.as_bytes().to_vec(),
            })
            .await
        {
            Ok(Reply::Attr(attr)) => {
                assert_eq!(attr.parent_id, parent);
                attr.item_id
            }
            other => panic!("lookup {name}: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_serves_a_scenery_file_from_the_ortho_union_fs() {
    let patches = tempfile::tempdir().unwrap();
    let patch_dir = patches.path().join("TestPatch");
    std::fs::create_dir_all(patch_dir.join("Earth nav data/+30-120")).unwrap();
    std::fs::write(
        patch_dir.join("Earth nav data/+30-120/+33-119.dsf"),
        DSF_CONTENTS,
    )
    .unwrap();
    let index = OrthoUnionIndexBuilder::new()
        .with_patches_dir(patches.path())
        .build()
        .unwrap();

    let (tx, _rx) = mpsc::channel::<JobRequest>(8);
    let fs = Fuse3OrthoUnionFS::new(index, Arc::new(ChannelDdsClient::new(tx)), 11_174_016);

    let (module_end, server_end) = UnixStream::pair().unwrap();
    tokio::spawn(serve_connection(Arc::new(fs), server_end));
    let mut module = Module {
        stream: module_end,
        next_id: 1,
    };

    // activate(): the root's attributes.
    let Ok(Reply::Attr(root)) = module
        .call(Request::GetAttr {
            item_id: ROOT_ITEM_ID,
        })
        .await
    else {
        panic!("root getattr failed");
    };
    assert_eq!(root.kind, ItemKind::Directory);

    // enumerateDirectory() with attributes: no dot entries, real children.
    let Ok(Reply::Dir(entries)) = module
        .call(Request::ReadDir {
            item_id: ROOT_ITEM_ID,
            cookie: 0,
            with_attrs: true,
        })
        .await
    else {
        panic!("root readdir failed");
    };
    let names: Vec<_> = entries
        .iter()
        .map(|e| String::from_utf8_lossy(&e.name).into_owned())
        .collect();
    assert!(names.contains(&"Earth nav data".to_string()), "{names:?}");
    assert!(!names.iter().any(|n| n == "." || n == ".."), "{names:?}");

    // Walk to the file and read it the way FSKit would.
    let dir = module.lookup(ROOT_ITEM_ID, "Earth nav data").await;
    let dir = module.lookup(dir, "+30-120").await;
    let file = module.lookup(dir, "+33-119.dsf").await;

    let Ok(Reply::Opened { fh }) = module.call(Request::Open { item_id: file }).await else {
        panic!("open failed");
    };
    let read = module
        .call(Request::Read {
            item_id: file,
            fh,
            offset: 0,
            length: 1 << 20,
        })
        .await;
    assert_eq!(
        read,
        Ok(Reply::Data(bytes::Bytes::from_static(DSF_CONTENTS)))
    );
    assert_eq!(
        module.call(Request::Release { item_id: file, fh }).await,
        Ok(Reply::Empty)
    );

    // A name that is not there is ENOENT, which FSKit turns into a failed lookup.
    assert_eq!(
        module
            .call(Request::Lookup {
                parent: ROOT_ITEM_ID,
                name: b"missing".to_vec(),
            })
            .await,
        Err(libc::ENOENT)
    );
}
