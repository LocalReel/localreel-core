//! Reference storage plugin: exposes one local directory over the sidecar
//! protocol. It exists to exercise the package/runtime contract end to end and
//! as the template for real providers (WebDAV, SMB, …).
//!
//! Configuration (`LOCALREEL_PLUGIN_CONFIG`):
//! - `root` — directory to serve (required);
//! - `public_http_base` — when set (e.g. `http://plugin.local`), entries are
//!   reported as HTTP URIs under that base instead of `file://` URIs, which is
//!   how a remote provider behaves.
//!
//! The host passes a token that must be echoed in `X-LocalReel-Token`.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use localreel_kernel::storage::MediaUri;
use serde_json::{json, Value};

struct Context {
    root: PathBuf,
    public_http_base: Option<String>,
}

fn main() {
    let token = std::env::var("LOCALREEL_PLUGIN_TOKEN").unwrap_or_default();
    let config: Value = std::env::var("LOCALREEL_PLUGIN_CONFIG")
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    let root = config
        .get("root")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .expect("LOCALREEL_PLUGIN_CONFIG must contain root");
    let root = root.canonicalize().expect("plugin root must exist");
    let context = Context {
        root,
        public_http_base: config
            .get("public_http_base")
            .and_then(Value::as_str)
            .map(|value| value.trim_end_matches('/').to_string()),
    };

    // Exit when the host closes stdin (normal shutdown or a crash that drops the
    // pipe), so no orphan process keeps the plugin port alive.
    std::thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 64];
        while matches!(stdin.read(&mut buffer), Ok(read) if read > 0) {}
        std::process::exit(0);
    });

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    println!("{}", json!({ "ready": true, "port": port }));
    std::io::stdout().flush().ok();

    for stream in listener.incoming().flatten() {
        let _ = handle(stream, &context, &token);
    }
}

fn handle(mut stream: TcpStream, context: &Context, token: &str) -> std::io::Result<()> {
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
        "/v1/stat" => stat(&mut stream, context, query),
        "/v1/list" => entries(&mut stream, context, query, false),
        "/v1/walk" => entries(&mut stream, context, query, true),
        "/v1/read" => read(&mut stream, context, query),
        _ => respond(&mut stream, 404, "text/plain", b"unknown route"),
    }
}

fn stat(stream: &mut TcpStream, context: &Context, query: &str) -> std::io::Result<()> {
    let Some(path) = resolve(context, query) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    let body = json!({
        "size": metadata.len(),
        "modified": modified(&metadata),
        "is_dir": metadata.is_dir(),
    });
    respond(stream, 200, "application/json", body.to_string().as_bytes())
}

fn entries(
    stream: &mut TcpStream,
    context: &Context,
    query: &str,
    recursive: bool,
) -> std::io::Result<()> {
    let Some(path) = resolve(context, query) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    let mut items = Vec::new();
    if recursive {
        for entry in walkdir::WalkDir::new(&path).follow_links(false) {
            let Ok(entry) = entry else { continue };
            if entry.path() == path {
                continue;
            }
            if let Some(item) = describe(context, entry.path()) {
                items.push(item);
            }
        }
    } else if let Ok(directory) = std::fs::read_dir(&path) {
        for entry in directory.flatten() {
            if let Some(item) = describe(context, &entry.path()) {
                items.push(item);
            }
        }
    } else {
        return respond(stream, 404, "text/plain", b"missing");
    }
    let body = json!({ "entries": items });
    respond(stream, 200, "application/json", body.to_string().as_bytes())
}

fn describe(context: &Context, path: &Path) -> Option<Value> {
    let metadata = std::fs::metadata(path).ok()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let uri = if let Some(base) = &context.public_http_base {
        let relative = path.strip_prefix(&context.root).ok()?;
        let suffix = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        format!("{base}/{suffix}")
    } else {
        MediaUri::Local(path.to_path_buf()).to_uri()
    };
    Some(json!({
        "uri": uri,
        "name": name,
        "is_dir": metadata.is_dir(),
        "size": metadata.len(),
        "modified": modified(&metadata),
    }))
}

fn read(stream: &mut TcpStream, context: &Context, query: &str) -> std::io::Result<()> {
    let Some(path) = resolve(context, query) else {
        return respond(stream, 404, "text/plain", b"missing");
    };
    let start = param(query, "start")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let len = param(query, "len")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return respond(stream, 404, "text/plain", b"missing"),
    };
    let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if start >= size {
        return respond(stream, 200, "application/octet-stream", b"");
    }
    file.seek(SeekFrom::Start(start))?;
    let length = len.min(size - start) as usize;
    let mut body = vec![0u8; length];
    let mut read = 0usize;
    while read < length {
        match file.read(&mut body[read..])? {
            0 => break,
            count => read += count,
        }
    }
    body.truncate(read);
    respond(stream, 200, "application/octet-stream", &body)
}

fn resolve(context: &Context, query: &str) -> Option<PathBuf> {
    let uri = param(query, "uri")?;
    let parsed = MediaUri::parse(&uri).ok()?;
    let path = match parsed {
        MediaUri::Local(path) => path,
        MediaUri::Http(url) => {
            let base = context.public_http_base.as_deref()?;
            let relative = url.strip_prefix(base)?.trim_start_matches('/');
            context.root.join(percent_decode(relative))
        }
        _ => return None,
    };
    let canonical = path.canonicalize().ok()?;
    if canonical.starts_with(&context.root) {
        Some(canonical)
    } else {
        None
    }
}

fn param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(key) == name {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
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

fn modified(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64)
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
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
