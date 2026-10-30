# FSKit Mounting on macOS (#210)

On macOS 26 and later XEarthLayer can mount its scenery filesystem through its
own FSKit module instead of macFUSE. That removes the kernel extension, and
with it the Recovery mode trip, "Reduced Security" and two reboots that
macFUSE needs on Apple Silicon. The user turns one switch on in System Settings.

This is the "native FSKit module" route from #210's alternatives. It was
chosen over teaching `fuse3` to reach macFUSE's own FSKit backend through
`MFMount` because it needs no macFUSE install at all, no changes to the `fuse3`
fork, and no reliance on an `MFMount` API that is undocumented.

## Architecture

```text
X-Plane ──► kernel ──► fskitd ──► XEarthLayerFS.appex  (Swift, sandboxed)
                                      │  bridge protocol, one Unix socket
                                      ▼
                xearthlayer ──► BridgeServer ──► Fuse3OrthoUnionFS
```

An FSKit module is an ExtensionKit app extension: a sandboxed process that
`fskitd` launches and that has to be shipped inside an app. It cannot host the
Rust filesystem itself, so it is a thin forwarder. Each FSKit operation becomes
one request to a `BridgeServer` running in the `xearthlayer` process, which
answers it by calling the same `fuse3::raw::Filesystem` method the macFUSE
session would. `Fuse3OrthoUnionFS` is unchanged, and the two mounts cannot
drift apart in behaviour.

| Piece | Where | Platform |
|-------|-------|----------|
| Wire protocol | `xearthlayer/src/fuse/bridge/protocol.rs` | all (tested in CI) |
| Bridge server | `xearthlayer/src/fuse/bridge/server.rs` | all (tested in CI) |
| Backend choice | `xearthlayer/src/fuse/fskit/backend.rs` | all (tested in CI) |
| `mount -F` driver | `xearthlayer/src/fuse/fskit/mount.rs` | macOS |
| FSKit module | `macos/fskit/Extension/` | macOS 26+ |
| Host app | `macos/fskit/App/` | macOS 26+ |
| Build and sign | `scripts/macos/build-fskit.sh`, `make fskit-macos` | macOS |

## Mounting

`fskit::mount::mount_spawned`:

1. Creates the resource directory `~/.xearthlayer/fskit/<mount folder>/` and
   binds `bridge.sock` in it. The server is accepting before `mount` runs,
   because the module connects while `mount` is in progress.
2. Runs `mount -F -t xearthlayer -o rdonly <resource dir> <mount point>`.
   `-F` sends the request to an FSKit module, and the resource directory reaches
   the module as an `FSPathURLResource`. Support for path resources is new in
   macOS 26 (`FSSupportsPathURLs`), and it is what lets a module mount with no
   block device behind it.
3. In the module, `probeResource` checks that the socket exists,
   `loadResource` connects to it and returns the volume, and `activate` fetches
   the root's attributes.
4. Returns a `SpawnedMountHandle` whose task runs `umount` and stops the
   server, so `MountManager` and the unmount fallbacks are unchanged.
   `is_path_mounted` reads `mount(8)` output and does not check the
   filesystem type, so it already covers this mount.

The resource directory's name becomes the volume name. Finder therefore shows
`zzXEL_ortho`, as `volname` makes it do under macFUSE (#305).

## Backend selection

`fuse.macos_backend` = `auto` | `fskit` | `macfuse`, decided at run time
because one binary must serve macOS 26 and earlier releases:

| Setting | Module state (`pluginkit -m -i …`) | Plan |
|---------|------------------------------------|------|
| `auto` | missing or user-disabled | macFUSE |
| `auto` | registered | FSKit, then macFUSE if the mount fails |
| `fskit` | any | FSKit only. A failure is an error, never a silent kext mount |
| `macfuse` | any | macFUSE only |

Falling back needs the filesystem value back after the FSKit attempt has
shared it with the bridge. `MountFailure` returns it once every bridge task
has let go (`reclaim`, which gives up after 2 s).

## Wire protocol

Little-endian frames: `u32` body length, then the body. Requests: `u64` id,
`u8` opcode, payload. Responses: `u64` id, `i32` errno (0 = success), payload
only on success. Ids let many requests be in flight on one connection; the
server runs each in its own task and writes replies in completion order.
Frames are capped at 64 MiB.

| Op | Request | Reply |
|----|---------|-------|
| 1 Lookup | parent id, name | attr |
| 2 GetAttr | item id | attr |
| 3 ReadDir | item id, cookie, with_attrs | up to 512 entries (id, kind, next cookie, name, optional attr) |
| 4 Open | item id | fh |
| 5 Read | item id, fh, offset, length | bytes |
| 6 Release | item id, fh | — |
| 7 StatFs | — | statfs |

Item ids are FSKit's (`FSItemIDRootDirectory` = 2, its parent = 1). The server
maps them to FUSE inodes by subtracting one, so the Swift side never sees FUSE
numbering. Directory cookies are the FUSE readdir offsets. With attributes
requested, `.` and `..` are dropped, as FSKit's `enumerateDirectory`
documentation requires.

`protocol.rs` pins the byte layout of a lookup frame in a test. The Swift codec
(`BridgeProtocol.swift`) mirrors the file, and a change to either has to be
made to both.

### Mapping FSKit onto the FUSE handle model

FSKit has no file handles: `openItem`/`closeItem` report the access modes still
open on an item. The module opens a bridge handle on the first open and
releases it when `closeItem` is called with no modes left, or when the item is
reclaimed. Reads pass that handle, so `Fuse3OrthoUnionFS` keeps memoising a
tile once per open (#234). A read with no handle uses `fh` 0, which the
filesystem already serves by resolving per call.

The module keeps one `FSItem` per id, because FSKit expects the same object
back for the same file, and drops it on `reclaimItem`. Attributes are cached on
the item from lookup and readdir. The tree does not change during a mount, so
`getAttributes` does not round-trip.

## Sandbox and signing

The module runs in the app sandbox. Its entitlements:

- `com.apple.developer.fskit.fsmodule`: a managed capability. macOS loads a
  module only when the signing team's provisioning profile grants it, so
  ad-hoc builds compile and verify but do not mount.
- `com.apple.security.temporary-exception.files.home-relative-path.read-write`
  for `/.xearthlayer/fskit/`: lets the module `connect(2)` to the socket.
  FSKit also hands it the resource directory, which may be enough on its own.
  If testing shows it is, the exception can go.

Release packaging does not build `XEarthLayer.app` yet. That needs the
project's Developer ID team to enable the FSKit Module capability for both
bundle ids (`net.defreyssi.xearthlayer`, `net.defreyssi.xearthlayer.fskit`) and
the profiles stored as release secrets.

## Behaviour differences from macFUSE

- **Concurrency limits.** `fuse.max_background` / `congestion_threshold` are
  FUSE INIT parameters, so they do nothing under FSKit. Concurrency is FSKit's,
  bounded by the bridge's per-request tasks.
- **Page cache.** FSKit reads go through the unified buffer cache, much as the
  macFUSE path does (see "Texture caching uses the kernel page cache" in
  `docs/macos.md` and #204). `VIRTUAL_DDS_OPEN_FLAGS` has no FSKit counterpart.
- **`init`/`destroy`** are never called: FSKit has no handshake.
  `Fuse3OrthoUnionFS` uses them only for logging.

## Open questions and validation still to do

These are what #210 makes a condition of adoption. They need a signed build on
a real machine:

1. **Throughput against the kext.** Every read crosses XPC (kernel to module)
   and then the socket (module to `xearthlayer`), and is copied at each hop. A
   10.66 MiB tile arrives in kernel-sized reads. This has to be measured against
   macFUSE on a scene load before `auto` should prefer FSKit by default.
2. **X-Plane's `mmap` texture reads** on an FSKit mount. `tests/fskit_smoke.rs`
   checks `mmap` on a plain file, and a flight is still needed.
3. **Whether the sandbox exception is needed**, or the path resource alone
   grants socket access.
4. **Distribution**: signing, notarizing and packaging `XEarthLayer.app`
   alongside the CLI. Whether the CLI should live inside the app is open.

## Tests

- `fuse::bridge::protocol` unit tests: every message round-trips, the byte
  layout is pinned, and malformed frames are rejected.
- `fuse::bridge::server` unit tests against an in-memory filesystem: id
  mapping, dot-entry handling, paging, handle lifecycle, concurrent requests,
  the oversized-frame guard, and socket lifecycle.
- `tests/fskit_bridge.rs`: the bridge serving the real `Fuse3OrthoUnionFS`
  over a socket pair. Runs everywhere, Linux CI included.
- `tests/fskit_smoke.rs` (ignored): the live mount through the installed
  module, with read, readdir, `mmap`, the mount table and unmount.
  `cargo test --test fskit_smoke -- --ignored --nocapture`.
- `tests/macfuse_smoke.rs` pins `MacosBackend::Macfuse`, so that it keeps
  testing the kext path on a machine where the module is installed.
