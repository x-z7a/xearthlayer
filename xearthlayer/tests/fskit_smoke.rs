//! Manual smoke test for the FSKit mount path (#210).
//!
//! Exercises the live FSKit backend end to end: kernel → FSKit → the
//! XEarthLayer module → bridge → `Fuse3OrthoUnionFS`. Mounts it over a temp
//! folder, reads a file back through the mount point, checks the volume name
//! and unmounts, with no macFUSE involved.
//!
//! Ignored by default because it needs macOS 26, XEarthLayer.app installed in
//! /Applications (built by `scripts/macos/build-fskit.sh` with a team identity)
//! and the file system extension turned on in System Settings.
//! Run with: `cargo test --test fskit_smoke -- --ignored --nocapture`

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use xearthlayer::executor::ChannelDdsClient;
use xearthlayer::fuse::fskit::MacosBackend;
use xearthlayer::fuse::fuse3::Fuse3OrthoUnionFS;
use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
use xearthlayer::runtime::JobRequest;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the XEarthLayer FSKit module installed and enabled (macOS 26+)"]
async fn ortho_union_fs_mounts_and_serves_a_file_through_fskit() {
    let patches = tempfile::tempdir().expect("patches tempdir");
    let patch_dir = patches.path().join("TestPatch");
    let dsf_rel = "Earth nav data/+30-120/+33-119.dsf";
    let dsf_contents = b"fake dsf payload for the fskit smoke test";
    std::fs::create_dir_all(patch_dir.join("Earth nav data/+30-120")).unwrap();
    std::fs::write(patch_dir.join(dsf_rel), dsf_contents).unwrap();
    let index = OrthoUnionIndexBuilder::new()
        .with_patches_dir(patches.path())
        .build()
        .expect("build ortho union index");

    // Named like the real mount so the volume name can be checked too.
    let scenery = tempfile::tempdir().expect("custom scenery tempdir");
    let mountpoint = scenery.path().join("zzXEL_fskit_smoke");
    std::fs::create_dir(&mountpoint).unwrap();

    let (tx, _rx) = mpsc::channel::<JobRequest>(8);
    let fs = Fuse3OrthoUnionFS::new(index, Arc::new(ChannelDdsClient::new(tx)), 11_174_016)
        .with_macos_backend(MacosBackend::Fskit);
    let handle = fs
        .mount_spawned(mountpoint.to_str().unwrap())
        .await
        .expect("FSKit mount must succeed");

    tokio::time::sleep(Duration::from_millis(500)).await;

    let read_back = std::fs::read(mountpoint.join(dsf_rel)).expect("read through the FSKit mount");
    assert_eq!(read_back, dsf_contents);

    let names: Vec<String> = std::fs::read_dir(&mountpoint)
        .expect("readdir the FSKit mount")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n == "Earth nav data"),
        "names: {names:?}"
    );

    // mmap is how X-Plane maps some scenery; it must work on an FSKit mount.
    let mapped = mmap_read(&mountpoint.join(dsf_rel));
    assert_eq!(mapped, dsf_contents);

    let mount_table = std::process::Command::new("/sbin/mount").output().unwrap();
    let mount_table = String::from_utf8_lossy(&mount_table.stdout);
    let line = mount_table
        .lines()
        .find(|l| l.contains("zzXEL_fskit_smoke"))
        .expect("mount must be listed");
    assert!(!line.contains("macfuse"), "mounted through macFUSE: {line}");

    handle.unmount().await.expect("FSKit unmount must succeed");
    assert!(
        !xearthlayer::system::is_path_mounted(&mountpoint),
        "unmount must remove the mount"
    );
}

/// Read a whole file through `mmap(2)`.
fn mmap_read(path: &std::path::Path) -> Vec<u8> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path).unwrap();
    let len = file.metadata().unwrap().len() as usize;
    unsafe {
        let addr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(
            addr,
            libc::MAP_FAILED,
            "{}",
            std::io::Error::last_os_error()
        );
        let bytes = std::slice::from_raw_parts(addr.cast::<u8>(), len).to_vec();
        libc::munmap(addr, len);
        bytes
    }
}
