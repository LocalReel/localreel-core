//! End-to-end: build a package, extract it, start the sidecar and read through
//! the `Storage` seam.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use localreel_kernel::storage::{MediaUri, Storage};
use localreel_plugin::{extract, inspect, PluginRuntime, PluginStorage};
use serde_json::json;
use zip::write::SimpleFileOptions;

const DEMO: &str = env!("CARGO_BIN_EXE_plugin-demo");

fn build_package(path: &Path, program: &str, manifest: &serde_json::Value) {
    let file = File::create(path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    writer
        .start_file("manifest.json", SimpleFileOptions::default())
        .unwrap();
    writer
        .write_all(serde_json::to_vec(manifest).unwrap().as_slice())
        .unwrap();
    writer
        .start_file(program, SimpleFileOptions::default())
        .unwrap();
    writer.write_all(&std::fs::read(DEMO).unwrap()).unwrap();
    writer.finish().unwrap();
}

fn manifest(program: &str, transport: &str) -> serde_json::Value {
    json!({
        "id": "demo",
        "name": "Reference storage plugin",
        "version": "0.1.0",
        "api_version": localreel_plugin::API_VERSION,
        "kind": "storage",
        "entry": { "program": program, "args": [], "transport": transport },
        "permissions": ["network"]
    })
}

#[test]
fn sidecar_serves_stat_list_walk_and_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("library");
    std::fs::create_dir_all(library.join("sub")).unwrap();
    std::fs::write(library.join("a.mp4"), b"abcdef").unwrap();
    std::fs::write(library.join("sub").join("b.mkv"), b"12345").unwrap();

    let program = format!("bin/plugin-demo{}", std::env::consts::EXE_SUFFIX);
    let package = dir.path().join("demo.lrplug");
    build_package(&package, &program, &manifest(&program, "http"));

    // Manifest-first: inspect does not extract anything.
    let inspected = inspect(&package).unwrap();
    assert_eq!(inspected.id, "demo");

    let install = dir.path().join("installed");
    let extracted = extract(&package, &install).unwrap();
    let executable = install.join(&extracted.entry.program);
    assert!(executable.is_file());

    let runtime = Arc::new(
        PluginRuntime::start(
            &executable,
            &extracted,
            &json!({ "root": library.to_str().unwrap() }),
            &localreel_plugin::PluginSandbox {
                working_dir: Some(install.clone()),
                roots: vec![library.to_string_lossy().into_owned()],
                host_url: None,
                limits: Default::default(),
            },
        )
        .unwrap(),
    );
    let storage = PluginStorage::new(runtime);

    let file_uri = MediaUri::Local(library.join("a.mp4"));
    let stat = storage.stat(&file_uri).unwrap();
    assert_eq!(stat.size, 6);
    assert!(!stat.is_dir);

    let mut listed: Vec<String> = storage
        .list(&MediaUri::Local(library.clone()))
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    listed.sort();
    assert_eq!(listed, vec!["a.mp4", "sub"]);

    let walked: Vec<String> = storage
        .walk(&MediaUri::Local(library.clone()))
        .unwrap()
        .map(|entry| entry.unwrap().name)
        .collect();
    assert!(walked.contains(&"a.mp4".to_string()));
    assert!(walked.contains(&"b.mkv".to_string()));

    let mut reader = storage.open_range(&file_uri, 1, 3).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"bcd");

    let missing = storage.stat(&MediaUri::Local(library.join("nope.mp4")));
    assert!(missing.is_err());
}

#[test]
fn a_package_with_a_newer_api_version_is_rejected_before_extraction() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("future.lrplug");
    let mut value = manifest("bin/plugin-demo", "http");
    value["api_version"] = json!("localreel.plugin/v99");
    build_package(&package, "bin/plugin-demo", &value);

    let error = inspect(&package).unwrap_err();
    assert!(matches!(
        error,
        localreel_plugin::PluginError::ApiVersion(_)
    ));
    let install = dir.path().join("installed");
    assert!(!install.exists());
}

#[test]
fn stdio_sidecar_serves_storage_without_a_tcp_port() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("library");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(library.join("a.mp4"), b"abcdef").unwrap();

    let program = format!("bin/plugin-demo{}", std::env::consts::EXE_SUFFIX);
    let package = dir.path().join("demo-stdio.lrplug");
    build_package(&package, &program, &manifest(&program, "stdio"));

    let install = dir.path().join("installed");
    let extracted = extract(&package, &install).unwrap();
    assert_eq!(extracted.entry.transport, "stdio");
    let executable = install.join(&extracted.entry.program);
    let runtime = Arc::new(
        PluginRuntime::start(
            &executable,
            &extracted,
            &json!({ "root": library.to_str().unwrap() }),
            &localreel_plugin::PluginSandbox {
                working_dir: Some(install.clone()),
                roots: vec![library.to_string_lossy().into_owned()],
                host_url: None,
                limits: Default::default(),
            },
        )
        .unwrap(),
    );
    assert!(runtime.is_stdio());
    let storage = PluginStorage::new(runtime);

    let file_uri = MediaUri::Local(library.join("a.mp4"));
    assert_eq!(storage.stat(&file_uri).unwrap().size, 6);
    let listed: Vec<String> = storage
        .list(&MediaUri::Local(library.clone()))
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(listed, vec!["a.mp4"]);

    let mut reader = storage.open_range(&file_uri, 1, 3).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"bcd");
}
