//! Mounting through XEarthLayer's FSKit module (macOS 26 and later).
//!
//! The mount is driven by `mount -F`, which hands a path resource to the
//! module. The resource is a per-mount directory holding the bridge socket,
//! and its name becomes the volume name Finder shows, matching what
//! `volname` gives a macFUSE mount (#305).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use fuse3::raw::Filesystem;
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use super::backend::{parse_pluginkit_listing, ModuleState, FSKIT_EXTENSION_POINT};
use crate::fuse::bridge::BridgeServer;
use crate::fuse::SpawnedMountHandle;

/// The type name the module registers (`FSShortName` in its `Info.plist`).
pub const FS_SHORT_NAME: &str = "xearthlayer";

/// Name of the bridge socket inside the resource directory. The module looks
/// for exactly this name.
pub const SOCKET_NAME: &str = "bridge.sock";

/// `sun_path` holds 104 bytes on macOS, including the terminating NUL.
const MAX_SOCKET_PATH: usize = 103;

/// How long to wait for the module to give up its last reference to the
/// filesystem after a failed mount, so that macFUSE can be tried with it.
const RECLAIM_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask pluginkit whether the module is installed and enabled.
pub fn module_state() -> ModuleState {
    match Command::new("/usr/bin/pluginkit")
        .args(["-m", "-p", FSKIT_EXTENSION_POINT])
        .output()
    {
        Ok(output) => parse_pluginkit_listing(&String::from_utf8_lossy(&output.stdout)),
        Err(e) => {
            debug!(error = %e, "pluginkit unavailable; treating the FSKit module as missing");
            ModuleState::Missing
        }
    }
}

/// The resource directory for a mount at `mountpoint`.
///
/// One directory per mount point, named after its folder, under
/// `~/.xearthlayer/fskit/`.
pub fn resource_dir(mountpoint: &Path) -> io::Result<PathBuf> {
    let name = mountpoint.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("mount point {} has no folder name", mountpoint.display()),
        )
    })?;
    Ok(crate::paths::config_dir().join("fskit").join(name))
}

/// A failed FSKit mount, carrying the filesystem back when it could be
/// reclaimed so that the caller can try macFUSE with it.
pub struct MountFailure<F> {
    pub error: io::Error,
    pub fs: Option<F>,
}

/// Mount `fs` at `mountpoint` through the FSKit module.
pub async fn mount_spawned<F>(
    fs: F,
    mountpoint: &Path,
) -> Result<SpawnedMountHandle, MountFailure<F>>
where
    F: Filesystem + Send + Sync + 'static,
{
    let fs = Arc::new(fs);
    let fail = |error: io::Error, fs: Arc<F>| async move {
        MountFailure {
            error,
            fs: reclaim(fs).await,
        }
    };

    let dir = match resource_dir(mountpoint) {
        Ok(dir) => dir,
        Err(e) => return Err(fail(e, fs).await),
    };
    let socket = dir.join(SOCKET_NAME);
    if socket.as_os_str().len() > MAX_SOCKET_PATH {
        let e = io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "bridge socket path {} is longer than the {MAX_SOCKET_PATH} bytes macOS allows",
                socket.display()
            ),
        );
        return Err(fail(e, fs).await);
    }

    let server = match BridgeServer::bind(&socket) {
        Ok(server) => server,
        Err(e) => return Err(fail(e, fs).await),
    };
    let (stop_tx, stop_rx) = oneshot::channel();
    let server_task = tokio::spawn(server.run(Arc::clone(&fs), stop_rx));

    // The module connects to the socket while `mount` runs, so the server
    // has to be accepting before the command starts.
    if let Err(e) = run_mount(&dir, mountpoint).await {
        let _ = stop_tx.send(());
        let _ = server_task.await;
        return Err(fail(e, fs).await);
    }
    drop(fs);

    info!(
        mountpoint = %mountpoint.display(),
        resource = %dir.display(),
        "Mounted through the FSKit module (no kernel extension)"
    );

    let (unmount_tx, unmount_rx) = oneshot::channel::<()>();
    let target = mountpoint.to_path_buf();
    let task = tokio::spawn(async move {
        let _ = unmount_rx.await;
        let result = run_unmount(&target).await;
        let _ = stop_tx.send(());
        let _ = server_task.await;
        result
    });

    Ok(SpawnedMountHandle::new(
        task,
        unmount_tx,
        mountpoint.to_path_buf(),
    ))
}

async fn run_mount(resource: &Path, mountpoint: &Path) -> io::Result<()> {
    let mut command = Command::new("/sbin/mount");
    command
        .args(["-F", "-t", FS_SHORT_NAME, "-o", "rdonly"])
        .arg(resource)
        .arg(mountpoint);
    run(command, "mount").await.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "{e}. Is the XEarthLayer file system extension enabled in System Settings > \
                 General > Login Items & Extensions > File System Extensions?"
            ),
        )
    })
}

async fn run_unmount(mountpoint: &Path) -> io::Result<()> {
    let mut command = Command::new("/sbin/umount");
    command.arg(mountpoint);
    run(command, "umount").await
}

/// Run a command on the blocking pool, turning a non-zero exit into an error
/// that carries its stderr.
async fn run(mut command: Command, what: &'static str) -> io::Result<()> {
    let output = tokio::task::spawn_blocking(move || command.output())
        .await
        .map_err(io::Error::other)??;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(io::Error::other(format!(
        "{what} failed ({}): {}",
        output.status,
        stderr.trim()
    )))
}

/// Take sole ownership of the filesystem back once every bridge task holding
/// it has finished, or give up after [`RECLAIM_TIMEOUT`].
async fn reclaim<F>(mut fs: Arc<F>) -> Option<F> {
    let deadline = tokio::time::Instant::now() + RECLAIM_TIMEOUT;
    loop {
        match Arc::try_unwrap(fs) {
            Ok(fs) => return Some(fs),
            Err(shared) if tokio::time::Instant::now() < deadline => {
                fs = shared;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => {
                warn!("FSKit bridge still holds the filesystem; macFUSE cannot be tried");
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resource_dir_is_named_after_the_mount_folder() {
        let dir = resource_dir(Path::new(
            "/Applications/X-Plane 12/Custom Scenery/zzXEL_ortho",
        ))
        .unwrap();
        assert_eq!(dir.file_name().unwrap(), "zzXEL_ortho");
        assert_eq!(dir.parent().unwrap().file_name().unwrap(), "fskit");
    }

    #[test]
    fn a_root_mount_point_has_no_resource_dir() {
        assert!(resource_dir(Path::new("/")).is_err());
    }

    #[tokio::test]
    async fn reclaim_returns_the_filesystem_once_unshared() {
        let fs = Arc::new(5u32);
        let clone = Arc::clone(&fs);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(clone);
        });
        assert_eq!(reclaim(fs).await, Some(5));
    }
}
