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
const BROKER_DEMO: &str = env!("CARGO_BIN_EXE_plugin-broker-demo");
// Only the sandbox tests read this binary, and those do not exist on macOS;
// without the gate the constant is dead code there and `-D warnings` fails.
#[cfg(any(windows, target_os = "linux"))]
const NET_DEMO: &str = env!("CARGO_BIN_EXE_plugin-net-demo");

fn build_package(path: &Path, program: &str, manifest: &serde_json::Value) {
    build_package_with_binary(path, program, manifest, &std::fs::read(DEMO).unwrap());
}

fn build_package_with_binary(
    path: &Path,
    program: &str,
    manifest: &serde_json::Value,
    binary: &[u8],
) {
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
    writer.write_all(binary).unwrap();
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
                app_container: false,
                sandbox_program: None,
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
                app_container: false,
                sandbox_program: None,
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

#[test]
fn stdio_broker_calls_travel_over_pipes() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        request.push_str(&String::from_utf8_lossy(&body));
        let body = r#"{"status":200,"content_type":"text/plain","body_b64":"aGk="}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
        request
    });

    let dir = tempfile::tempdir().unwrap();
    let program = format!("bin/plugin-broker-demo{}", std::env::consts::EXE_SUFFIX);
    let package = dir.path().join("broker-demo.lrplug");
    let mut manifest = manifest(&program, "stdio");
    manifest["id"] = json!("broker-demo");
    build_package_with_binary(
        &package,
        &program,
        &manifest,
        &std::fs::read(BROKER_DEMO).unwrap(),
    );
    let install = dir.path().join("installed");
    let extracted = extract(&package, &install).unwrap();
    let executable = install.join(&extracted.entry.program);
    let runtime = PluginRuntime::start(
        &executable,
        &extracted,
        &json!({}),
        &localreel_plugin::PluginSandbox {
            working_dir: Some(install.clone()),
            roots: Vec::new(),
            host_url: Some(format!("http://{address}")),
            limits: Default::default(),
            app_container: false,
            sandbox_program: None,
        },
    )
    .unwrap();

    runtime.schedule("{}").unwrap();
    let result = std::fs::read_to_string(install.join("broker-result.txt")).unwrap();
    assert_eq!(result, "hi");

    let request = server.join().unwrap();
    assert!(
        request.contains("POST /api/v1/plugin-broker/fetch"),
        "{request}"
    );
    assert!(request.contains("x-localreel-plugin:"), "{request}");
    assert!(request.contains("https://example.com/data"), "{request}");
}

#[cfg(target_os = "linux")]
fn sandbox_is_available() -> bool {
    localreel_plugin::runtime::sandbox_support().unshare_present
}

#[cfg(windows)]
fn sandbox_is_available() -> bool {
    true
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn appcontainer_plugin_uses_the_stdio_broker() {
    if !sandbox_is_available() {
        eprintln!("skipping: OS isolation is unavailable");
        return;
    }
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        request.push_str(&String::from_utf8_lossy(&body));
        let body = r#"{"status":200,"content_type":"text/plain","body_b64":"aGk="}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
        request
    });

    let dir = tempfile::tempdir().unwrap();
    let program = format!("bin/plugin-broker-demo{}", std::env::consts::EXE_SUFFIX);
    let package = dir.path().join("broker-demo-appcontainer.lrplug");
    let mut manifest = manifest(&program, "stdio");
    manifest["id"] = json!("broker-demo-ac");
    build_package_with_binary(
        &package,
        &program,
        &manifest,
        &std::fs::read(BROKER_DEMO).unwrap(),
    );
    let install = dir.path().join("installed");
    let extracted = extract(&package, &install).unwrap();
    let executable = install.join(&extracted.entry.program);
    let runtime = PluginRuntime::start(
        &executable,
        &extracted,
        &json!({}),
        &localreel_plugin::PluginSandbox {
            working_dir: Some(install.clone()),
            roots: Vec::new(),
            host_url: Some(format!("http://{address}")),
            limits: Default::default(),
            app_container: true,
            sandbox_program: Some(std::path::PathBuf::from(env!(
                "CARGO_BIN_EXE_plugin-sandbox"
            ))),
        },
    )
    .unwrap();

    runtime.schedule("{}").unwrap();
    let request = server.join().unwrap();
    assert!(
        request.contains("POST /api/v1/plugin-broker/fetch"),
        "{request}"
    );
    assert!(request.contains("x-localreel-plugin:"), "{request}");
    assert!(request.contains("https://example.com/data"), "{request}");
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn appcontainer_denies_the_plugins_own_socket() {
    if !sandbox_is_available() {
        eprintln!("skipping: OS isolation is unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let program = format!("bin/plugin-net-demo{}", std::env::consts::EXE_SUFFIX);
    let package = dir.path().join("net-demo.lrplug");
    let mut manifest = manifest(&program, "stdio");
    manifest["id"] = json!("net-demo");
    build_package_with_binary(
        &package,
        &program,
        &manifest,
        &std::fs::read(NET_DEMO).unwrap(),
    );
    let install = dir.path().join("installed");
    let extracted = extract(&package, &install).unwrap();
    let executable = install.join(&extracted.entry.program);
    let runtime = PluginRuntime::start(
        &executable,
        &extracted,
        &json!({}),
        &localreel_plugin::PluginSandbox {
            working_dir: Some(install.clone()),
            roots: Vec::new(),
            host_url: None,
            limits: Default::default(),
            app_container: true,
            sandbox_program: Some(std::path::PathBuf::from(env!(
                "CARGO_BIN_EXE_plugin-sandbox"
            ))),
        },
    )
    .unwrap();

    runtime.schedule("{}").unwrap();
    let stderr = runtime.stderr_text(4096);
    assert!(stderr.contains("net: blocked"), "{stderr}");
}

fn spawn_fs_fixture() -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            let request = String::from_utf8_lossy(&body).to_string();
            requests.push(request.clone());
            let value: serde_json::Value = serde_json::from_str(&request).unwrap();
            let reply = match value["op"].as_str().unwrap_or_default() {
                "stat" => json!({"size": 6, "modified": null, "is_dir": false}),
                "list" => json!({"entries": [{
                    "uri": "file:///library/a.mp4",
                    "name": "a.mp4",
                    "is_dir": false,
                    "size": 6,
                    "modified": null
                }]}),
                "read" => json!({"body_b64": "YmNk"}),
                other => panic!("unexpected fs op {other}"),
            };
            let body = reply.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
        requests
    });
    (format!("http://{address}"), server)
}

fn storage_plugin_serves_from_fs_broker(app_container: bool) {
    let (host_url, server) = spawn_fs_fixture();
    let dir = tempfile::tempdir().unwrap();
    let program = format!("bin/plugin-demo{}", std::env::consts::EXE_SUFFIX);
    let package = dir.path().join("demo-broker.lrplug");
    build_package(&package, &program, &manifest(&program, "stdio"));
    let install = dir.path().join("installed");
    let extracted = extract(&package, &install).unwrap();
    let executable = install.join(&extracted.entry.program);
    let runtime = Arc::new(
        PluginRuntime::start(
            &executable,
            &extracted,
            &json!({ "fs_mode": "broker" }),
            &localreel_plugin::PluginSandbox {
                working_dir: Some(install.clone()),
                roots: Vec::new(),
                host_url: Some(host_url),
                limits: Default::default(),
                app_container,
                sandbox_program: if app_container {
                    Some(std::path::PathBuf::from(env!(
                        "CARGO_BIN_EXE_plugin-sandbox"
                    )))
                } else {
                    None
                },
            },
        )
        .unwrap(),
    );
    let storage = PluginStorage::new(runtime);

    let file_uri = MediaUri::Local(std::path::PathBuf::from("unused.mp4"));
    assert_eq!(storage.stat(&file_uri).unwrap().size, 6);
    let listed = storage
        .list(&MediaUri::Local(std::path::PathBuf::from("unused")))
        .unwrap();
    assert_eq!(listed[0].name, "a.mp4");

    let mut reader = storage.open_range(&file_uri, 1, 3).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"bcd");

    let requests = server.join().unwrap();
    assert!(requests.iter().all(|body| body.contains("\"op\"")));
    assert!(requests.iter().any(|body| body.contains("\"stat\"")));
    assert!(requests.iter().any(|body| body.contains("\"list\"")));
    assert!(requests.iter().any(|body| body.contains("\"read\"")));
}

#[test]
fn storage_plugin_uses_fs_broker_over_stdio() {
    storage_plugin_serves_from_fs_broker(false);
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn appcontainer_storage_plugin_uses_fs_broker() {
    if !sandbox_is_available() {
        eprintln!("skipping: OS isolation is unavailable");
        return;
    }
    storage_plugin_serves_from_fs_broker(true);
}
