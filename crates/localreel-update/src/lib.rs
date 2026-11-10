//! The self-update path: signed manifest, verified download, installer hand-off.
//!
//! LocalReel does not replace its own binary behind the operator's back. A
//! release is described by `update.json`, signed with an ed25519 key whose
//! public half is compiled in below. The server fetches the manifest and its
//! detached signature, verifies both, downloads the installer for this
//! platform, checks its size and SHA-256, and only then offers a button. The
//! replacement itself is the per-user setup doing what it already does; this
//! crate decides *whether* and *what* to run, and nothing else.
//!
//! Everything here is pure: parsing, verification, comparison, detection and
//! the download. The state machine and the process handling live in the API,
//! so this crate stays testable without a server and without a network.

pub mod download;
pub mod install;
pub mod manifest;
pub mod version;

pub use download::{download, download_with_progress, DownloadError, MAX_ASSET_BYTES};
pub use install::{detect, plan, ApplyPlan, InstallKind};
pub use manifest::{
    verify_signature, Asset, AssetKind, Edition, Manifest, ManifestError, Platform,
};
pub use version::{compare, is_newer, normalize};

/// Public half of the release signing key, hex.
///
/// The private half exists only as the `UPDATE_SIGNING_KEY` secret of the
/// release workflow. Anyone can read this file; nobody can produce a manifest
/// that verifies against it.
pub const PUBLIC_KEY_HEX: &str = include_str!("../pubkey.txt");
