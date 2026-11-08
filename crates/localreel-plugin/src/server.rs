//! Host-facing HTTP side of the sidecar protocol.
//!
//! A plugin implements [`Handler`] and calls [`run`]: the module binds loopback,
//! prints the one `ready` line, checks the per-run token, and serves `stat`,
//! `list`, `walk` and `read` until the host closes stdin.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use serde_json::json;

/// One directory entry returned by a provider.
#[derive(Debug, Clone)]
pub struct Entry {
    pub uri: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<i64>,
}

/// Metadata for one object.
#[derive(Debug, Clone)]
pub struct Stat {
    pub size: u64,
    pub modified: Option<i64>,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerError {
    NotFound,
    Failed(String),
}

impl HandlerError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

/// A storage provider implemented by a plugin.
pub trait Handler: Send + Sync {
    fn stat(&self, uri: &str) -> Result<Stat, HandlerError>;
    fn list(&self, uri: &str, recursive: bool) -> Result<Vec<Entry>, HandlerError>;
    fn read(&self, uri: &str, start: u64, len: u64) -> Result<Vec<u8>, HandlerError>;
    /// One host-scheduled callback. The default refuses, so existing storage
    /// plugins keep their old behaviour.
    fn schedule(&self, _payload: &str) -> Result<(), HandlerError> {
        Err(HandlerError::NotFound)
    }
}

/// Serve the plugin protocol on loopback until the process is terminated.
pub fn run(handler: Box<dyn Handler>) -> std::io::Result<()> {
    let token = std::env::var("LOCALREEL_PLUGIN_TOKEN").unwrap_or_default();

    // Exit when the host closes stdin (normal shutdown or a crash that drops the
    // pipe), so no orphan process keeps the plugin port alive.
    std::thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 64];
        while matches!(stdin.read(&mut buffer), Ok(read) if read > 0) {}
        std::process::exit(0);
    });

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    println!("{}", json!({ "ready": true, "port": port }));
    std::io::stdout().flush()?;

    for stream in listener.incoming().flatten() {
        let _ = handle(stream, handler.as_ref(), &token);
    }
    Ok(())
}

fn handle(mut stream: TcpStream, handler: &dyn Handler, token: &str) -> std::io::Result<()> {
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
        return respond(&mut stream, 403, "text/plain", b"forbidden");
    }

    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    match path {
        "/v1/health" => respond(&mut stream, 200, "application/json", b"{\"ok\":true}"),
        "/v1/stat" => stat(&mut stream, handler, query),
        "/v1/list" => entries(&mut stream, handler, query, false),
        "/v1/walk" => entries(&mut stream, handler, query, true),
        "/v1/read" => read(&mut stream, handler, query),
        "/v1/schedule" => schedule(&mut stream, handler, query),
        _ => respond(&mut stream, 404, "text/plain", b"unknown route"),
    }
}

fn stat(stream: &mut TcpStream, handler: &dyn Handler, query: &str) -> std::io::Result<()> {
    let Some(uri) = param(query, "uri") else {
        return respond(stream, 400, "text/plain", b"missing uri");
    };
    match handler.stat(&uri) {
        Ok(stat) => {
            let body = json!({
                "size": stat.size,
                "modified": stat.modified,
                "is_dir": stat.is_dir,
            });
            respond(stream, 200, "application/json", body.to_string().as_bytes())
        }
        Err(error) => error_response(stream, error),
    }
}

fn entries(
    stream: &mut TcpStream,
    handler: &dyn Handler,
    query: &str,
    recursive: bool,
) -> std::io::Result<()> {
    let Some(uri) = param(query, "uri") else {
        return respond(stream, 400, "text/plain", b"missing uri");
    };
    match handler.list(&uri, recursive) {
        Ok(entries) => {
            let items: Vec<_> = entries
                .into_iter()
                .map(|entry| {
                    json!({
                        "uri": entry.uri,
                        "name": entry.name,
                        "is_dir": entry.is_dir,
                        "size": entry.size,
                        "modified": entry.modified,
                    })
                })
                .collect();
            let body = json!({ "entries": items });
            respond(stream, 200, "application/json", body.to_string().as_bytes())
        }
        Err(error) => error_response(stream, error),
    }
}

fn read(stream: &mut TcpStream, handler: &dyn Handler, query: &str) -> std::io::Result<()> {
    let Some(uri) = param(query, "uri") else {
        return respond(stream, 400, "text/plain", b"missing uri");
    };
    let start = param(query, "start")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let len = param(query, "len")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    match handler.read(&uri, start, len) {
        Ok(bytes) => respond(stream, 200, "application/octet-stream", &bytes),
        Err(error) => error_response(stream, error),
    }
}

fn schedule(stream: &mut TcpStream, handler: &dyn Handler, query: &str) -> std::io::Result<()> {
    let payload = param(query, "payload").unwrap_or_default();
    match handler.schedule(&payload) {
        Ok(()) => respond(stream, 200, "application/json", b"{\"ok\":true}"),
        Err(error) => error_response(stream, error),
    }
}

fn error_response(stream: &mut TcpStream, error: HandlerError) -> std::io::Result<()> {
    match error {
        HandlerError::NotFound => respond(stream, 404, "text/plain", b"not found"),
        HandlerError::Failed(message) => respond(stream, 500, "text/plain", message.as_bytes()),
    }
}

/// Read one query parameter, percent-decoding the value.
pub fn param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(key) == name {
            return Some(percent_decode(value));
        }
    }
    None
}

pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}
