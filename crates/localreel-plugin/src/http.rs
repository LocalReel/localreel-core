//! A minimal HTTP/1.1 GET client for the loopback plugin protocol.
//!
//! Sidecars are local, speak plain HTTP and return a fixed `Content-Length`, so
//! a small blocking client is enough and avoids a TLS/HTTP dependency stack.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::manifest::PluginError;

const MAX_HEADER_BYTES: usize = 16 * 1024;

pub struct Response {
    pub status: u16,
    pub content_length: Option<u64>,
    reader: BufReader<TcpStream>,
}

impl Read for Response {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buffer)
    }
}

/// Send one GET request and return the response headers plus a body reader.
pub fn get(
    address: SocketAddr,
    token: &str,
    path: &str,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<Response, PluginError> {
    let mut stream = TcpStream::connect_timeout(&address, connect_timeout)
        .map_err(|error| PluginError::Request(format!("cannot reach plugin: {error}")))?;
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|error| PluginError::Request(error.to_string()))?;
    stream
        .set_write_timeout(Some(connect_timeout))
        .map_err(|error| PluginError::Request(error.to_string()))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-LocalReel-Token: {token}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| PluginError::Request(format!("cannot send request: {error}")))?;
    stream
        .flush()
        .map_err(|error| PluginError::Request(error.to_string()))?;

    let mut reader = BufReader::new(stream);
    let mut header_bytes = 0usize;
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(|error| PluginError::Request(format!("cannot read status: {error}")))?;
    header_bytes += status_line.len();
    let status = parse_status(&status_line)?;
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| PluginError::Request(format!("cannot read headers: {error}")))?;
        if read == 0 {
            break;
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(PluginError::Request(
                "response headers are too large".into(),
            ));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<u64>().ok();
            }
        }
    }
    Ok(Response {
        status,
        content_length,
        reader,
    })
}

fn parse_status(line: &str) -> Result<u16, PluginError> {
    let mut parts = line.split_whitespace();
    let _protocol = parts.next();
    let code = parts
        .next()
        .ok_or_else(|| PluginError::Request("missing status code".into()))?
        .parse::<u16>()
        .map_err(|_| PluginError::Request("invalid status code".into()))?;
    Ok(code)
}

/// Percent-encode a query parameter value (RFC 3986 unreserved characters kept).
pub fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_values() {
        assert_eq!(
            encode("file:///D:/a b.mp4"),
            "file%3A%2F%2F%2FD%3A%2Fa%20b.mp4"
        );
    }

    #[test]
    fn parses_status_lines() {
        assert_eq!(parse_status("HTTP/1.1 200 OK\r\n").unwrap(), 200);
        assert!(parse_status("garbage").is_err());
    }
}
