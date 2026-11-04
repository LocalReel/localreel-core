//! Sidecar plugins for LocalReel.
//!
//! A plugin package is a zip with a `manifest.json` at its root. The manifest is
//! validated before extraction or execution; the host then starts the entry
//! program as a child process and talks to it over loopback HTTP. Storage
//! plugins implement the [`localreel_kernel::storage::Storage`] contract through
//! [`PluginStorage`].

pub mod http;
pub mod manifest;
pub mod package;
pub mod runtime;
pub mod storage;

pub use manifest::{Manifest, PluginError, PluginKind, API_VERSION};
pub use package::{extract, inspect};
pub use runtime::PluginRuntime;
pub use storage::PluginStorage;
