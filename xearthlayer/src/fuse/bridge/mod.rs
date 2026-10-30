//! Bridge between the macOS FSKit module and the FUSE filesystem.
//!
//! On macOS 26 and later XEarthLayer can mount through its own FSKit module
//! instead of macFUSE, which removes the kernel extension and the Recovery
//! mode approval it needs (#210). FSKit modules are sandboxed app extensions
//! written in Swift, so the module cannot host the Rust filesystem itself. It
//! forwards each FSKit operation over a Unix socket to a [`BridgeServer`] in
//! the XEarthLayer process, which answers it from the same
//! [`fuse3::raw::Filesystem`] implementation the macFUSE mount uses.
//!
//! ```text
//! X-Plane ──► kernel ──► FSKit module (Swift, sandboxed)
//!                              │  bridge protocol over a Unix socket
//!                              ▼
//!                        BridgeServer ──► Fuse3OrthoUnionFS
//! ```
//!
//! The protocol and server are platform-neutral so that their tests run on
//! every platform; only the mount itself (`fuse::fskit`) is macOS-specific.

pub mod protocol;
mod server;

pub use server::{serve_connection, BridgeServer};
