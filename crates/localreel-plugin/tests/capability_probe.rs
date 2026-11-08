//! Acceptance probe for the capability boundary.
//!
//! Phase 1 records what a plugin asks for; it does not yet stop a plugin from
//! opening its own socket. This test makes that gap observable and gives phase 3
//! a regression target: once the transport/sandbox lands, this test must fail
//! (or be rewritten to assert the connection is refused).

use std::io::Read;
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;

use localreel_plugin::{Manifest, PluginRuntime, PluginSandbox};
use serde_json::json;

const PROBE: &str = env!("CARGO_BIN_EXE_plugin-probe");

fn probe_manifest() -> Manifest {
    Manifest::parse(
        br#"{
            "id": "probe",
            "name": "Capability probe",
            "version": "0.1.0",
            "api_version": "localreel.plugin/v1",
            "kind": "storage",
            "entry": { "program": "plugin-probe", "args": [] },
            "permissions": ["network"]
        }"#,
    )
    .unwrap()
}

#[test]
#[ignore = "documents the pre-sandbox bypass; phase 3 flips this expectation"]
fn probe_can_open_a_raw_socket_until_sandboxed() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let accept = std::thread::spawn(move || listener.accept());

    let runtime = PluginRuntime::start(
        Path::new(PROBE),
        &probe_manifest(),
        &json!({ "probe_addr": address.to_string() }),
        &PluginSandbox {
            working_dir: None,
            roots: Vec::new(),
        },
    )
    .expect("probe plugin starts");

    let (mut stream, _) = accept.join().unwrap().expect("probe connects");
    let mut bytes = [0u8; 5];
    stream.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"probe");

    let mut stderr = String::new();
    for _ in 0..40 {
        stderr = runtime.stderr_text(4096);
        if stderr.contains("raw_socket=") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        stderr.contains("raw_socket=connected"),
        "probe should be able to open a raw socket in phase 1: {stderr}"
    );

    drop(runtime);
}
