//! The signed release manifest (`update.json`).
//!
//! Shape, published next to the release assets:
//!
//! ```json
//! {
//!   "version": "0.5.7",
//!   "channel": "stable",
//!   "min_version": "0.5.0",
//!   "notes_url": "https://github.com/LocalReel/LocalReel/releases/tag/v0.5.7",
//!   "published_at": "2026-11-09T14:36:17Z",
//!   "assets": [
//!     { "platform": "windows-x64", "kind": "setup", "url": "https://…/localreel-0.5.7-x64-setup.exe",
//!       "size": 7012345, "sha256": "…" }
//!   ]
//! }
//! ```
//!
//! `update.json.sig` holds the detached ed25519 signature over the exact bytes
//! of `update.json`, hex encoded. The public half is compiled into the binary,
//! so a manifest served by a mirror or an attacker is worthless without the
//! matching private key.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::version::{self, normalize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("the manifest is not JSON: {0}")]
    Json(String),
    #[error("the manifest names no version")]
    MissingVersion,
    #[error("the manifest names {0:?}, which is not a version number")]
    BadVersion(String),
    #[error("the signature is not 64 bytes of hex")]
    BadSignature,
    #[error("the public key is not 32 bytes of hex")]
    BadPublicKey,
    #[error("the signature does not match the manifest")]
    SignatureMismatch,
    #[error("this release has no build for {0}")]
    NoAssetFor(Platform),
    #[error("this release needs {required} or newer; install that one manually first")]
    TooOld { required: String },
    #[error("release {latest} is not newer than the installed {current}")]
    NotNewer { latest: String, current: String },
    #[error("the asset url {0:?} is not https")]
    InsecureUrl(String),
}

/// What is installed: the server alone, or the server plus the desktop shell.
///
/// The two are different downloads and must stay different upgrades; the
/// edition is read from the install directory, not from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Server,
    Desktop,
}

/// The platforms a release can carry a build for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    WindowsX64,
    WindowsArm64,
    LinuxX64,
    LinuxArm64,
    MacosX64,
    MacosArm64,
}

impl Platform {
    /// The build for the platform this binary runs on.
    pub fn current() -> Platform {
        if cfg!(target_os = "windows") {
            if cfg!(target_arch = "aarch64") {
                Platform::WindowsArm64
            } else {
                Platform::WindowsX64
            }
        } else if cfg!(target_os = "macos") {
            if cfg!(target_arch = "aarch64") {
                Platform::MacosArm64
            } else {
                Platform::MacosX64
            }
        } else if cfg!(target_arch = "aarch64") {
            Platform::LinuxArm64
        } else {
            Platform::LinuxX64
        }
    }

    /// The release asset for this platform, as installed by the setup or the
    /// package scripts.
    pub fn slug(self) -> &'static str {
        match self {
            Platform::WindowsX64 => "windows-x64",
            Platform::WindowsArm64 => "windows-arm64",
            Platform::LinuxX64 => "linux-x64",
            Platform::LinuxArm64 => "linux-arm64",
            Platform::MacosX64 => "macos-x64",
            Platform::MacosArm64 => "macos-arm64",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.slug())
    }
}

/// What kind of file an asset is. Only the first three are ever applied
/// automatically; the rest exist so the UI can offer the right download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetKind {
    Setup,
    /// The installer that also carries the desktop shell. Kept apart from
    /// `Setup` so a server-only installation is never handed 300 MB of
    /// Chromium it did not ask for.
    SetupDesktop,
    Msi,
    /// The desktop edition as an MSI, for administrators who deploy by policy.
    MsiDesktop,
    Zip,
    Tarball,
    Deb,
    Appimage,
    Dmg,
    Spk,
}

impl AssetKind {
    /// Lower is more likely to be what the updater wants on a desktop.
    fn preference(self) -> u8 {
        match self {
            AssetKind::Setup => 0,
            AssetKind::SetupDesktop => 1,
            AssetKind::Msi => 2,
            AssetKind::MsiDesktop => 3,
            AssetKind::Zip => 4,
            AssetKind::Tarball => 5,
            _ => 6,
        }
    }

    /// The name this kind uses in the manifest and the API.
    pub fn slug(self) -> &'static str {
        match self {
            AssetKind::Setup => "setup",
            AssetKind::SetupDesktop => "setup-desktop",
            AssetKind::Msi => "msi",
            AssetKind::MsiDesktop => "msi-desktop",
            AssetKind::Zip => "zip",
            AssetKind::Tarball => "tarball",
            AssetKind::Deb => "deb",
            AssetKind::Appimage => "appimage",
            AssetKind::Dmg => "dmg",
            AssetKind::Spk => "spk",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub platform: Platform,
    pub kind: AssetKind,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    /// The oldest version that may upgrade straight to this one.
    #[serde(default)]
    pub min_version: Option<String>,
    #[serde(default)]
    pub notes_url: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

fn default_channel() -> String {
    "stable".to_string()
}

impl Manifest {
    /// Verify the detached signature and parse the manifest.
    pub fn load(
        bytes: &[u8],
        signature_hex: &str,
        public_key_hex: &str,
    ) -> Result<Self, ManifestError> {
        verify_signature(bytes, signature_hex, public_key_hex)?;
        let manifest = Self::parse(bytes)?;
        Ok(manifest)
    }

    /// Parse without verifying. Only for the tests and for reading a manifest
    /// that was already verified.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|error| ManifestError::Json(format!("not UTF-8: {error}")))?;
        let mut manifest: Manifest =
            serde_json::from_str(text).map_err(|error| ManifestError::Json(error.to_string()))?;
        manifest.version = normalize(&manifest.version);
        if manifest.version.is_empty() {
            return Err(ManifestError::MissingVersion);
        }
        if !manifest
            .version
            .split('.')
            .next()
            .is_some_and(|head| head.chars().all(|c| c.is_ascii_digit()))
        {
            return Err(ManifestError::BadVersion(manifest.version));
        }
        Ok(manifest)
    }

    /// The build this platform should download, preferring an installer over a
    /// plain archive.
    pub fn asset_for(&self, platform: Platform) -> Option<&Asset> {
        self.assets
            .iter()
            .filter(|asset| asset.platform == platform)
            .min_by_key(|asset| asset.kind.preference())
    }

    pub fn asset_for_current(&self) -> Option<&Asset> {
        self.asset_for(Platform::current())
    }

    /// The installer for one edition.
    ///
    /// A desktop installation must not be updated with the thin installer (the
    /// shell would vanish) and a server installation must not be updated with
    /// the desktop one (300 MB nobody asked for), so the two kinds are looked
    /// up separately. `Setup` remains the fallback for a desktop installation
    /// only when no desktop build exists for this release.
    pub fn asset_for_edition(&self, platform: Platform, edition: Edition) -> Option<&Asset> {
        let preferred = match edition {
            Edition::Desktop => [AssetKind::SetupDesktop, AssetKind::Setup],
            Edition::Server => [AssetKind::Setup, AssetKind::SetupDesktop],
        };
        preferred.iter().find_map(|kind| {
            self.assets
                .iter()
                .find(|asset| asset.platform == platform && asset.kind == *kind)
        })
    }

    /// Which edition is installed in `install_dir`: the shell sits beside the
    /// server, so its presence is the answer.
    pub fn edition_of(install_dir: &std::path::Path) -> Edition {
        if install_dir.join("desktop").join("LocalReel.exe").is_file() {
            Edition::Desktop
        } else {
            Edition::Server
        }
    }

    /// Refuse a release that would be a downgrade, or one this installation is
    /// too old to jump to.
    pub fn check_against(&self, current: &str) -> Result<(), ManifestError> {
        let current = normalize(current);
        if !version::is_newer(&self.version, &current) {
            return Err(ManifestError::NotNewer {
                latest: self.version.clone(),
                current,
            });
        }
        if let Some(minimum) = &self.min_version {
            if version::is_newer(minimum, &current) {
                return Err(ManifestError::TooOld {
                    required: minimum.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Verify a detached ed25519 signature over `bytes`.
pub fn verify_signature(
    bytes: &[u8],
    signature_hex: &str,
    public_key_hex: &str,
) -> Result<(), ManifestError> {
    let key = decode_fixed::<32>(public_key_hex).ok_or(ManifestError::BadPublicKey)?;
    let signature = decode_fixed::<64>(signature_hex).ok_or(ManifestError::BadSignature)?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| ManifestError::BadPublicKey)?;
    let signature = Signature::from_bytes(&signature);
    key.verify_strict(bytes, &signature)
        .map_err(|_| ManifestError::SignatureMismatch)
}

fn decode_fixed<const N: usize>(hex: &str) -> Option<[u8; N]> {
    let hex = hex.trim();
    if hex.len() != N * 2 {
        return None;
    }
    let mut bytes = [0u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let high = nibble(hex.as_bytes()[index * 2])?;
        let low = nibble(hex.as_bytes()[index * 2 + 1])?;
        *byte = (high << 4) | low;
    }
    Some(bytes)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Hex encoding, matching `localreel-plugin`'s convention.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const BODY: &str = r#"{
        "version": "0.5.7",
        "channel": "stable",
        "min_version": "0.5.0",
        "notes_url": "https://example.invalid/notes",
        "assets": [
            { "platform": "windows-x64", "kind": "zip", "url": "https://example.invalid/a.zip",
              "size": 10, "sha256": "00" },
            { "platform": "windows-x64", "kind": "setup", "url": "https://example.invalid/a-setup.exe",
              "size": 20, "sha256": "11" },
            { "platform": "linux-x64", "kind": "tarball", "url": "https://example.invalid/a.tar.gz",
              "size": 30, "sha256": "22" }
        ]
    }"#;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn sign(body: &str) -> (String, String) {
        let key = key();
        let signature = key.sign(body.as_bytes());
        (
            to_hex(&signature.to_bytes()),
            to_hex(key.verifying_key().as_bytes()),
        )
    }

    #[test]
    fn parses_and_normalizes_the_version() {
        let manifest = Manifest::parse(BODY.as_bytes()).unwrap();
        assert_eq!(manifest.version, "0.5.7");
        assert_eq!(manifest.channel, "stable");
        assert_eq!(manifest.assets.len(), 3);
    }

    const TWO_EDITIONS: &str = r#"{
        "version": "0.5.7",
        "channel": "stable",
        "min_version": "0.5.0",
        "notes_url": "https://example.invalid/notes",
        "assets": [
            { "platform": "windows-x64", "kind": "setup", "url": "https://example.invalid/thin-setup.exe",
              "size": 10, "sha256": "00" },
            { "platform": "windows-x64", "kind": "setup-desktop", "url": "https://example.invalid/desktop-setup.exe",
              "size": 20, "sha256": "11" }
        ]
    }"#;

    #[test]
    fn each_edition_updates_with_its_own_installer() {
        let manifest = Manifest::parse(TWO_EDITIONS.as_bytes()).unwrap();
        let server = manifest
            .asset_for_edition(Platform::WindowsX64, Edition::Server)
            .unwrap();
        assert_eq!(server.kind, AssetKind::Setup);
        let desktop = manifest
            .asset_for_edition(Platform::WindowsX64, Edition::Desktop)
            .unwrap();
        assert_eq!(desktop.kind, AssetKind::SetupDesktop);
    }

    #[test]
    fn a_missing_edition_asset_falls_back_rather_than_failing() {
        let manifest = Manifest::parse(BODY.as_bytes()).unwrap();
        // BODY only has the thin setup: a desktop installation still updates,
        // it just comes back without a shell until the next release.
        assert_eq!(
            manifest
                .asset_for_edition(Platform::WindowsX64, Edition::Desktop)
                .unwrap()
                .kind,
            AssetKind::Setup
        );
    }

    #[test]
    fn the_installed_edition_is_read_from_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Manifest::edition_of(dir.path()), Edition::Server);
        std::fs::create_dir(dir.path().join("desktop")).unwrap();
        std::fs::write(dir.path().join("desktop").join("LocalReel.exe"), b"stub").unwrap();
        assert_eq!(Manifest::edition_of(dir.path()), Edition::Desktop);
    }

    #[test]
    fn accepts_a_matching_signature() {
        let (signature, public_key) = sign(BODY);
        let manifest = Manifest::load(BODY.as_bytes(), &signature, &public_key).unwrap();
        assert_eq!(manifest.version, "0.5.7");
    }

    #[test]
    fn rejects_a_tampered_manifest() {
        let (signature, public_key) = sign(BODY);
        let tampered = BODY.replace("0.5.7", "0.5.8");
        assert_eq!(
            Manifest::load(tampered.as_bytes(), &signature, &public_key),
            Err(ManifestError::SignatureMismatch)
        );
    }

    #[test]
    fn rejects_malformed_signatures_and_keys() {
        assert_eq!(
            Manifest::load(BODY.as_bytes(), "zz", &"00".repeat(32)),
            Err(ManifestError::BadSignature)
        );
        let (signature, _) = sign(BODY);
        assert_eq!(
            Manifest::load(BODY.as_bytes(), &signature, "00"),
            Err(ManifestError::BadPublicKey)
        );
    }

    #[test]
    fn prefers_the_installer_over_the_archive() {
        let manifest = Manifest::parse(BODY.as_bytes()).unwrap();
        let asset = manifest.asset_for(Platform::WindowsX64).unwrap();
        assert_eq!(asset.kind, AssetKind::Setup);
        assert_eq!(asset.size, 20);
        assert!(manifest.asset_for(Platform::MacosArm64).is_none());
    }

    #[test]
    fn refuses_downgrades_and_too_old_installs() {
        let manifest = Manifest::parse(BODY.as_bytes()).unwrap();
        assert!(manifest.check_against("0.5.6").is_ok());
        assert_eq!(
            manifest.check_against("0.5.7"),
            Err(ManifestError::NotNewer {
                latest: "0.5.7".into(),
                current: "0.5.7".into()
            })
        );
        assert_eq!(
            manifest.check_against("0.4.9"),
            Err(ManifestError::TooOld {
                required: "0.5.0".into()
            })
        );
    }

    #[test]
    fn hex_round_trips() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(decode_fixed::<3>("000fff"), Some([0, 15, 255]));
        assert_eq!(decode_fixed::<3>("00"), None);
        assert_eq!(decode_fixed::<3>("00ffzz"), None);
    }
}
