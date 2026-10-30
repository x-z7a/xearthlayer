//! Kext-less macOS mounting through XEarthLayer's FSKit module (#210).
//!
//! [`backend`] decides between FSKit and macFUSE and builds everywhere so its
//! tests run in CI; [`mount`] performs the FSKit mount and exists only on
//! macOS. The module itself lives in `macos/fskit/`, and the protocol it
//! speaks in [`crate::fuse::bridge`].

pub mod backend;
#[cfg(target_os = "macos")]
pub mod mount;

pub use backend::{MacosBackend, ModuleState, Plan, Transport};
