//! A deliberately minimal probe plugin for the capability model.
//!
//! It asks for `net.fetch` in its test manifest, then tries to open a raw TCP
//! socket to an address the host did **not** broker. Before phase 2/3 there is
//! nothing that can stop it; the probe makes that gap observable instead of
//! leaving it as a claim in a design document.
//!
//! Usage (from the integration test or a manual package):
//!   LOCALREEL_PLUGIN_CONFIG='{"probe_addr":"127.0.0.1:PORT"}' plugin-probe
//!
//! It writes `probe raw_socket=connected|refused` to stderr and then serves
//! `/v1/health` so the normal sidecar handshake succeeds.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use serde_json::{json, Value};

fn main() {
    let token = std::env::var("LOCALREEL_PLUGIN_TOKEN").unwrap_or_default();
    let config: Value = std::env::var("LOCALREEL_PLUGIN_CONFIG")
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    let probe_addr = config
        .get("probe_addr")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<SocketAddr>().ok());

    let result = match probe_addr {
        Some(address) => match TcpStream::connect_timeout(&address, Duration::from_secs(2)) {
            Ok(mut stream) => {
                let _ = stream.write_all(b"probe");
                "connected"
            }
            Err(_) => "refused",
        },
        None => "not_configured",
    };
    // stderr is the documented diagnostic channel; stdout stays for the ready
    // line and nothing else.
    eprintln!("probe raw_socket={result}");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    println!("{}", json!({ "ready": true, "port": port }));
    std::io::stdout().flush().ok();

    for stream in listener.incoming().flatten() {
        let _ = handle(stream, &token);
    }
}

fn handle(mut stream: TcpStream, token: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut authorized = token.is_empty();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("x-localreel-token") && value.trim() == token {
                authorized = true;
            }
        }
    }
    if !authorized {
        return respond(&mut stream, 403, b"forbidden");
    }
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let (path, _query) = target.split_once('?').unwrap_or((&target, ""));
    match path {
        "/v1/health" => respond(&mut stream, 200, b"{\"ok\":true}"),
        _ => respond(&mut stream, 404, b"not found"),
    }
}

fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

// Keep the `Read` import used even when the compiler is in a build without the
// integration test that reads the probe's stderr.
#[allow(dead_code)]
fn _assert_read<R: Read>(_reader: R) {}
