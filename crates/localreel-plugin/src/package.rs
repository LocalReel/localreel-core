//! Plugin packages: a zip with a manifest at its root.
//!
//! `inspect` reads only the manifest; `extract` validates the package again and
//! unpacks it into a directory owned by the host. Entry names are confined to
//! the destination, symlinks are rejected, and both the archive size and the
//! expanded size are bounded.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use zip::ZipArchive;

use crate::manifest::{
    Manifest, PluginError, MAX_EXTRACTED_BYTES, MAX_MANIFEST_BYTES, MAX_PACKAGE_BYTES,
    MAX_PACKAGE_ENTRIES,
};

/// Read the manifest without extracting anything.
pub fn inspect(path: &Path) -> Result<Manifest, PluginError> {
    let mut archive = open(path)?;
    let manifest = read_manifest(&mut archive)?;
    ensure_program_present(&mut archive, &manifest)?;
    Ok(manifest)
}

/// Extract a package into `destination`, returning its validated manifest.
///
/// The destination is created by the caller; an existing directory is reused so
/// upgrading a plugin replaces its files. Anything suspicious aborts the
/// extraction (leaving partial files, which the caller may delete).
pub fn extract(path: &Path, destination: &Path) -> Result<Manifest, PluginError> {
    let mut archive = open(path)?;
    let manifest = read_manifest(&mut archive)?;
    ensure_program_present(&mut archive, &manifest)?;
    std::fs::create_dir_all(destination).map_err(|error| {
        PluginError::Io(format!("cannot create {}: {error}", destination.display()))
    })?;
    let root = destination.canonicalize().map_err(|error| {
        PluginError::Io(format!("cannot resolve {}: {error}", destination.display()))
    })?;
    let mut seen = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| PluginError::Archive(error.to_string()))?;
        let name = entry
            .enclosed_name()
            .ok_or_else(|| PluginError::UnsafeEntry(entry.name().to_string()))?;
        if entry.is_dir() {
            continue;
        }
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                return Err(PluginError::UnsafeEntry(entry.name().to_string()));
            }
        }
        if !seen.insert(name.clone()) {
            return Err(PluginError::UnsafeEntry(format!(
                "duplicate entry {}",
                name.display()
            )));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_EXTRACTED_BYTES {
            return Err(PluginError::ExtractedTooLarge);
        }
        let target = root.join(&name);
        if !target.starts_with(&root) {
            return Err(PluginError::UnsafeEntry(entry.name().to_string()));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                PluginError::Io(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        let mut output = File::create(&target).map_err(|error| {
            PluginError::Io(format!("cannot write {}: {error}", target.display()))
        })?;
        std::io::copy(&mut entry, &mut output).map_err(|error| {
            PluginError::Io(format!("cannot extract {}: {error}", target.display()))
        })?;
        output.flush().ok();
    }
    let program = root.join(Path::new(&manifest.entry.program));
    if !program.is_file() {
        return Err(PluginError::Missing(manifest.entry.program.clone()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&program)
            .map_err(|error| PluginError::Io(error.to_string()))?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&program, permissions).ok();
    }
    Ok(manifest)
}

/// Result of packing a plugin directory.
#[derive(Debug, Clone)]
pub struct PackReport {
    pub manifest: Manifest,
    pub path: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub sha256: String,
}

/// Build a `.lrplug` from a directory that contains `manifest.json`.
///
/// The archive is deterministic (sorted entries, fixed timestamps), so packing
/// the same directory twice yields the same SHA-256. The manifest is validated
/// before anything is written, symlinks and non-relative paths are rejected, and
/// the finished package is opened again with [`inspect`] as a self-check.
pub fn pack(source: &Path, output: &Path) -> Result<PackReport, PluginError> {
    let source = source.canonicalize().map_err(|error| {
        PluginError::Io(format!("cannot resolve {}: {error}", source.display()))
    })?;
    if !source.is_dir() {
        return Err(PluginError::Io(format!(
            "{} is not a directory",
            source.display()
        )));
    }
    let manifest_bytes = std::fs::read(source.join("manifest.json"))
        .map_err(|_| PluginError::Missing("manifest.json".into()))?;
    let manifest = Manifest::parse(&manifest_bytes)?;
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(PluginError::ManifestTooLarge);
    }

    let program = PathBuf::from(&manifest.entry.program);
    let mut files = collect_package_files(&source)?;
    if !files.iter().any(|path| path == &program) {
        return Err(PluginError::Missing(manifest.entry.program.clone()));
    }
    files.sort();
    if let Some(index) = files
        .iter()
        .position(|path| path == Path::new("manifest.json"))
    {
        let manifest_entry = files.remove(index);
        files.insert(0, manifest_entry);
    }
    if files.len() > MAX_PACKAGE_ENTRIES {
        return Err(PluginError::TooManyEntries);
    }

    let file_name = output
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin.lrplug".to_string());
    let temp = output.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    let write_result = write_package(&temp, &source, &files, &program);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    let size = std::fs::metadata(&temp)
        .map_err(|error| PluginError::Io(error.to_string()))?
        .len();
    if size > MAX_PACKAGE_BYTES {
        let _ = std::fs::remove_file(&temp);
        return Err(PluginError::PackageTooLarge);
    }
    if output.exists() {
        std::fs::remove_file(output).map_err(|error| {
            PluginError::Io(format!("cannot replace {}: {error}", output.display()))
        })?;
    }
    std::fs::rename(&temp, output).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        PluginError::Io(format!("cannot write {}: {error}", output.display()))
    })?;

    let inspected = inspect(output)?;
    if inspected.id != manifest.id || inspected.version != manifest.version {
        return Err(PluginError::Archive(
            "packed manifest does not round-trip".into(),
        ));
    }
    let sha256 = sha256_file(output)?;
    Ok(PackReport {
        manifest,
        path: output.to_path_buf(),
        files: files.len(),
        bytes: size,
        sha256,
    })
}

fn collect_package_files(root: &Path) -> Result<Vec<PathBuf>, PluginError> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| PluginError::Io(error.to_string()))?;
        if entry.path() == root {
            continue;
        }
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            return Err(PluginError::UnsafeEntry(format!(
                "symlink {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| PluginError::UnsafeEntry(entry.path().display().to_string()))?
            .to_path_buf();
        if relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(PluginError::UnsafeEntry(relative.display().to_string()));
        }
        files.push(relative);
    }
    if files.is_empty() {
        return Err(PluginError::Missing("manifest.json".into()));
    }
    Ok(files)
}

fn write_package(
    temp: &Path,
    source: &Path,
    files: &[PathBuf],
    program: &Path,
) -> Result<(), PluginError> {
    let file = File::create(temp)
        .map_err(|error| PluginError::Io(format!("cannot create {}: {error}", temp.display())))?;
    let mut writer = zip::ZipWriter::new(file);
    let mut total = 0u64;
    for relative in files {
        let bytes = std::fs::read(source.join(relative)).map_err(|error| {
            PluginError::Io(format!("cannot read {}: {error}", relative.display()))
        })?;
        total = total.saturating_add(bytes.len() as u64);
        if total > MAX_EXTRACTED_BYTES {
            return Err(PluginError::ExtractedTooLarge);
        }
        let name = relative.to_string_lossy().replace('\\', "/");
        let mode = if relative == program || relative.starts_with("bin") {
            0o755
        } else {
            0o644
        };
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(zip::DateTime::default())
            .unix_permissions(mode);
        writer
            .start_file(name, options)
            .map_err(|error| PluginError::Archive(error.to_string()))?;
        writer
            .write_all(&bytes)
            .map_err(|error| PluginError::Io(error.to_string()))?;
    }
    let file = writer
        .finish()
        .map_err(|error| PluginError::Archive(error.to_string()))?;
    file.sync_all()
        .map_err(|error| PluginError::Io(error.to_string()))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, PluginError> {
    use sha2::{Digest, Sha256};

    let mut file = File::open(path)
        .map_err(|error| PluginError::Io(format!("cannot read {}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|error| PluginError::Io(error.to_string()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

fn open(path: &Path) -> Result<ZipArchive<File>, PluginError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| PluginError::Io(format!("cannot read {}: {error}", path.display())))?;
    if metadata.len() > MAX_PACKAGE_BYTES {
        return Err(PluginError::PackageTooLarge);
    }
    let file = File::open(path)
        .map_err(|error| PluginError::Io(format!("cannot open {}: {error}", path.display())))?;
    let archive = ZipArchive::new(file).map_err(|error| PluginError::Archive(error.to_string()))?;
    if archive.len() > MAX_PACKAGE_ENTRIES {
        return Err(PluginError::TooManyEntries);
    }
    Ok(archive)
}

fn read_manifest(archive: &mut ZipArchive<File>) -> Result<Manifest, PluginError> {
    let candidates = archive
        .file_names()
        .filter(|name| *name == "manifest.json")
        .count();
    if candidates == 0 {
        return Err(PluginError::Missing("manifest.json".into()));
    }
    if candidates > 1 {
        return Err(PluginError::UnsafeEntry("duplicate manifest.json".into()));
    }
    let entry = archive
        .by_name("manifest.json")
        .map_err(|error| PluginError::Archive(error.to_string()))?;
    if entry.size() as usize > MAX_MANIFEST_BYTES {
        return Err(PluginError::ManifestTooLarge);
    }
    let mut bytes = Vec::with_capacity(entry.size() as usize);
    entry
        .take(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| PluginError::Io(error.to_string()))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(PluginError::ManifestTooLarge);
    }
    Manifest::parse(&bytes)
}

fn ensure_program_present(
    archive: &mut ZipArchive<File>,
    manifest: &Manifest,
) -> Result<(), PluginError> {
    let program: PathBuf = PathBuf::from(&manifest.entry.program);
    let mut found = false;
    for name in archive.file_names() {
        let path = Path::new(name);
        if path == program {
            found = true;
            break;
        }
    }
    if !found {
        return Err(PluginError::Missing(manifest.entry.program.clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{PluginKind, API_VERSION};
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn manifest_json(id: &str) -> String {
        serde_json::json!({
            "id": id,
            "name": "Test plugin",
            "version": "0.1.0",
            "api_version": API_VERSION,
            "kind": "storage",
            "entry": { "program": "bin/plugin", "args": [] },
            "permissions": ["network"]
        })
        .to_string()
    }

    fn package(path: &Path, extra: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("manifest.json", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(manifest_json("demo").as_bytes()).unwrap();
        writer
            .start_file("bin/plugin", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"#!/bin/sh\n").unwrap();
        for (name, bytes) in extra {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn inspects_without_extracting() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("demo.lrplug");
        package(&archive, &[]);
        let manifest = inspect(&archive).unwrap();
        assert_eq!(manifest.id, "demo");
        assert_eq!(manifest.kind, PluginKind::Storage);
        assert!(manifest.entry.program == "bin/plugin");
    }

    #[test]
    fn extracts_the_package() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("demo.lrplug");
        package(&archive, &[("data/readme.txt", b"hello")]);
        let out = dir.path().join("out");
        let manifest = extract(&archive, &out).unwrap();
        assert_eq!(manifest.id, "demo");
        assert!(out.join("bin/plugin").is_file());
        assert_eq!(
            std::fs::read(out.join("data/readme.txt")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn rejects_path_traversal_entries() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("evil.lrplug");
        package(&archive, &[("../escape.txt", b"x")]);
        let out = dir.path().join("out");
        let error = extract(&archive, &out).unwrap_err();
        assert!(matches!(error, PluginError::UnsafeEntry(_)));
    }

    #[test]
    fn rejects_a_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("empty.lrplug");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("readme.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"nothing").unwrap();
        writer.finish().unwrap();
        assert!(matches!(inspect(&archive), Err(PluginError::Missing(_))));
    }

    #[test]
    fn rejects_a_missing_entry_program() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("noprog.lrplug");
        let file = File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("manifest.json", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(manifest_json("demo").as_bytes()).unwrap();
        writer.finish().unwrap();
        assert!(matches!(inspect(&archive), Err(PluginError::Missing(_))));
    }

    fn plugin_directory(root: &Path) -> PathBuf {
        let dir = root.join("plugin");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest_json("demo")).unwrap();
        std::fs::write(dir.join("bin/plugin"), b"#!/bin/sh\necho ready\n").unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data/readme.txt"), b"hello").unwrap();
        dir
    }

    #[test]
    fn packs_a_plugin_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_directory(temp.path());
        let output = temp.path().join("demo.lrplug");
        let report = pack(&dir, &output).unwrap();
        assert_eq!(report.manifest.id, "demo");
        assert_eq!(report.files, 3);
        assert_eq!(report.sha256.len(), 64);
        assert!(output.is_file());

        let manifest = inspect(&output).unwrap();
        assert_eq!(manifest.id, "demo");
        let extracted = temp.path().join("out");
        extract(&output, &extracted).unwrap();
        assert_eq!(
            std::fs::read(extracted.join("data/readme.txt")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn packing_the_same_directory_is_deterministic() {
        let temp = tempfile::tempdir().unwrap();
        let dir = plugin_directory(temp.path());
        let first = pack(&dir, &temp.path().join("first.lrplug")).unwrap();
        let second = pack(&dir, &temp.path().join("second.lrplug")).unwrap();
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(first.bytes, second.bytes);
    }

    #[test]
    fn pack_rejects_a_missing_entry_program() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("plugin");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest_json("demo")).unwrap();
        let error = pack(&dir, &temp.path().join("bad.lrplug")).unwrap_err();
        assert!(matches!(error, PluginError::Missing(_)));
    }
}
