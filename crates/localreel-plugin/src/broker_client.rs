//! Plugin-side client for the host's capability broker.
//!
//! A plugin should not grow its own HTTP policy. This client speaks to the host
//! broker with the per-run token and decodes the base64 body; host-side grants,
//! scope checks, SSRF policy and audit stay in one place.
//!
//! Two transports are supported: loopback HTTP for the default mode, and the
//! plugin's stdin/stdout when the host marks the plugin as `stdio`. The stdio
//! mode is what lets an AppContainer plugin reach the broker without a socket.

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerReply {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

impl BrokerReply {
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, String> {
        serde_json::from_slice(&self.body)
            .map_err(|error| format!("broker body is not valid JSON: {error}"))
    }
}

#[derive(Clone)]
enum Transport {
    Http {
        base_url: String,
        agent: ureq::Agent,
    },
    Stdio,
}

/// A small blocking client. Clone cheaply: the HTTP agent pools internally.
#[derive(Clone)]
pub struct BrokerClient {
    transport: Transport,
    token: String,
    next_id: std::sync::Arc<AtomicU64>,
}

impl BrokerClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self {
            transport: Transport::Http {
                base_url,
                agent: ureq::AgentBuilder::new()
                    .timeout(Duration::from_secs(60))
                    .build(),
            },
            token: token.into(),
            next_id: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }

    /// Read the host's environment.
    ///
    /// `LOCALREEL_PLUGIN_BROKER=stdio` selects the inherited pipe; otherwise the
    /// historical `LOCALREEL_PLUGIN_HOST_URL` HTTP mode is used. Returns `None`
    /// when the host provided no broker at all.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("LOCALREEL_PLUGIN_TOKEN").ok()?;
        if std::env::var("LOCALREEL_PLUGIN_BROKER").as_deref() == Ok("stdio") {
            return Some(Self {
                transport: Transport::Stdio,
                token,
                next_id: std::sync::Arc::new(AtomicU64::new(1)),
            });
        }
        let base_url = std::env::var("LOCALREEL_PLUGIN_HOST_URL").ok()?;
        Some(Self::new(base_url, token))
    }

    /// Fetch a URL through the host broker.
    pub fn fetch(&self, url: &str, headers: &[(&str, &str)]) -> Result<BrokerReply, String> {
        self.fetch_with_secret(url, None, headers)
    }

    /// Fetch with a host-held credential. The plugin names a handle; it never
    /// sees the value, and the host injects it only after checking its own
    /// `secret.use` grant.
    pub fn fetch_with_secret(
        &self,
        url: &str,
        secret: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<BrokerReply, String> {
        let mut map = BTreeMap::new();
        for (name, value) in headers {
            map.insert((*name).to_string(), (*value).to_string());
        }
        let mut payload = serde_json::json!({
            "url": url,
            "method": "GET",
            "headers": map,
        });
        if let Some(handle) = secret {
            payload["secret"] = Value::String(handle.to_string());
        }
        let (status, body) = self.request(
            "POST",
            "/api/v1/plugin-broker/fetch",
            payload.to_string().as_bytes(),
        )?;
        if status != 200 {
            return Err(format!(
                "broker refused the call ({status}): {}",
                broker_error_message(&String::from_utf8_lossy(&body))
            ));
        }
        parse_reply(&String::from_utf8_lossy(&body))
    }

    /// Ask the host for a periodic callback (`GET /v1/schedule`).
    pub fn request_schedule(&self, interval_secs: u64, payload: &Value) -> Result<Value, String> {
        let body = serde_json::json!({
            "interval_secs": interval_secs,
            "payload": payload,
        });
        let (status, response) = self.request(
            "POST",
            "/api/v1/plugin-broker/schedule",
            body.to_string().as_bytes(),
        )?;
        if status != 200 {
            return Err(format!(
                "schedule.request refused the call ({status}): {}",
                broker_error_message(&String::from_utf8_lossy(&response))
            ));
        }
        serde_json::from_slice(&response)
            .map_err(|error| format!("schedule.response is not JSON: {error}"))
    }

    /// Upload one blob into the host-managed namespace.
    pub fn put_blob(&self, name: &str, bytes: &[u8]) -> Result<Value, String> {
        let path = format!("/api/v1/plugin-broker/blob/{name}");
        let (status, response) = self.request("PUT", &path, bytes)?;
        if status != 200 {
            return Err(format!(
                "blob.write refused the blob ({status}): {}",
                broker_error_message(&String::from_utf8_lossy(&response))
            ));
        }
        serde_json::from_slice(&response)
            .map_err(|error| format!("blob.write response is not JSON: {error}"))
    }

    pub fn get_blob(&self, name: &str) -> Result<Vec<u8>, String> {
        let path = format!("/api/v1/plugin-broker/blob/{name}");
        let (status, body) = self.request("GET", &path, &[])?;
        if status == 200 {
            if body.len() as u64 > 16 * 1024 * 1024 {
                return Err("blob.read exceeded the 16 MiB client cap".to_string());
            }
            Ok(body)
        } else {
            Err(format!(
                "blob.read refused the call ({status}): {}",
                broker_error_message(&String::from_utf8_lossy(&body))
            ))
        }
    }

    pub fn delete_blob(&self, name: &str) -> Result<bool, String> {
        let path = format!("/api/v1/plugin-broker/blob/{name}");
        let (status, response) = self.request("DELETE", &path, &[])?;
        if status != 200 {
            return Err(format!(
                "blob.delete refused the call ({status}): {}",
                broker_error_message(&String::from_utf8_lossy(&response))
            ));
        }
        let value: Value = serde_json::from_slice(&response)
            .map_err(|error| format!("blob.delete response is not JSON: {error}"))?;
        Ok(value
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    pub fn fetch_json<T: DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<T, String> {
        self.fetch(url, headers)?.json()
    }

    /// `fs.read` over the host broker. The plugin only sends a `file://` URI;
    /// the host resolves it against the granted roots.
    pub fn fs_stat(&self, uri: &str) -> Result<Value, String> {
        self.fs_call("stat", uri, 0, 0)
    }

    pub fn fs_list(&self, uri: &str) -> Result<Value, String> {
        self.fs_call("list", uri, 0, 0)
    }

    pub fn fs_read(&self, uri: &str, start: u64, len: u64) -> Result<Vec<u8>, String> {
        let value = self.fs_call("read", uri, start, len)?;
        let encoded = value
            .get("body_b64")
            .and_then(Value::as_str)
            .ok_or_else(|| "fs.read response has no body_b64".to_string())?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("fs.read body is not base64: {error}"))
    }

    fn fs_call(&self, op: &str, uri: &str, start: u64, len: u64) -> Result<Value, String> {
        let body = serde_json::json!({
            "op": op,
            "uri": uri,
            "start": start,
            "len": len,
        });
        let (status, response) = self.request(
            "POST",
            "/api/v1/plugin-broker/fs",
            body.to_string().as_bytes(),
        )?;
        if status != 200 {
            return Err(format!(
                "fs.{op} refused the call ({status}): {}",
                broker_error_message(&String::from_utf8_lossy(&response))
            ));
        }
        serde_json::from_slice(&response)
            .map_err(|error| format!("fs.{op} response is not JSON: {error}"))
    }

    /// Stage normalized titles through `catalog.write(draft)`.
    pub fn write_catalog<T: Serialize>(&self, items: &[T]) -> Result<usize, String> {
        let payload = serde_json::json!({ "items": items });
        let (status, response) = self.request(
            "POST",
            "/api/v1/plugin-broker/catalog",
            payload.to_string().as_bytes(),
        )?;
        if status != 200 {
            return Err(format!(
                "catalog.write refused the batch ({status}): {}",
                broker_error_message(&String::from_utf8_lossy(&response))
            ));
        }
        let value: Value = serde_json::from_slice(&response)
            .map_err(|error| format!("catalog.write response is not JSON: {error}"))?;
        value
            .get("written")
            .and_then(Value::as_u64)
            .and_then(|written| usize::try_from(written).ok())
            .ok_or_else(|| "catalog.write response has no written count".to_string())
    }

    fn request(&self, method: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
        match &self.transport {
            Transport::Http { base_url, agent } => {
                let url = format!("{base_url}{path}");
                let mut request = agent
                    .request(method, &url)
                    .set("x-localreel-plugin", &self.token);
                if !body.is_empty() {
                    let content_type = if path.contains("/blob/") {
                        "application/octet-stream"
                    } else {
                        "application/json"
                    };
                    request = request.set("content-type", content_type);
                }
                let response = match request.send_bytes(body) {
                    Ok(response) => response,
                    Err(ureq::Error::Status(status, response)) => {
                        return Ok((status, read_body(response)));
                    }
                    Err(error) => return Err(format!("broker request failed: {error}")),
                };
                Ok((response.status(), read_body(response)))
            }
            Transport::Stdio => self.stdio_request(method, path, body),
        }
    }

    fn stdio_request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = serde_json::json!({
            "type": "broker.request",
            "id": id,
            "method": method,
            "path": path,
            "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
        })
        .to_string();
        {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            writeln!(output, "{line}").map_err(|error| format!("broker pipe write: {error}"))?;
            output
                .flush()
                .map_err(|error| format!("broker pipe flush: {error}"))?;
        }

        let stdin = std::io::stdin();
        loop {
            let mut response = String::new();
            let read = stdin
                .lock()
                .read_line(&mut response)
                .map_err(|error| format!("broker pipe read: {error}"))?;
            if read == 0 {
                return Err("broker pipe closed before a reply".to_string());
            }
            let value: Value = match serde_json::from_str(&response) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if value.get("type").and_then(Value::as_str) != Some("broker.reply")
                || value.get("id").and_then(Value::as_u64) != Some(id)
            {
                continue;
            }
            let status = value
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok())
                .ok_or_else(|| "broker reply has no status".to_string())?;
            let body = value
                .get("body_b64")
                .and_then(Value::as_str)
                .map(|encoded| {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|error| format!("broker reply body is not base64: {error}"))
                })
                .transpose()?
                .unwrap_or_default();
            return Ok((status, body));
        }
    }
}

fn read_body(response: ureq::Response) -> Vec<u8> {
    let mut body = Vec::new();
    let _ = response
        .into_reader()
        .take(32 * 1024 * 1024)
        .read_to_end(&mut body);
    body
}

fn parse_reply(text: &str) -> Result<BrokerReply, String> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("broker response is not JSON: {error}"))?;
    let status = value
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .ok_or_else(|| "broker response has no status".to_string())?;
    let content_type = value
        .get("content_type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let body_b64 = value
        .get("body_b64")
        .and_then(Value::as_str)
        .ok_or_else(|| "broker response has no body_b64".to_string())?;
    let body = base64::engine::general_purpose::STANDARD
        .decode(body_b64)
        .map_err(|error| format!("broker body is not valid base64: {error}"))?;
    Ok(BrokerReply {
        status,
        content_type,
        body,
    })
}

fn broker_error_message(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn spawn_broker(reply: String) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap_or(0);
                    }
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            request.push_str(&String::from_utf8_lossy(&body));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            request
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn parses_a_base64_reply() {
        let body = STANDARD.encode(b"{\"ok\":true}");
        let reply = parse_reply(&format!(
            r#"{{"status":200,"content_type":"application/json","body_b64":"{body}"}}"#
        ))
        .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.content_type.as_deref(), Some("application/json"));
        assert!(reply.json::<Value>().unwrap()["ok"].as_bool().unwrap());
    }

    #[test]
    fn rejects_missing_fields_and_bad_base64() {
        assert!(parse_reply("{}").is_err());
        assert!(parse_reply(r#"{"status":200,"body_b64":"!!!"}"#).is_err());
    }

    #[test]
    fn extracts_the_host_error_message() {
        assert_eq!(
            broker_error_message(r#"{"error":{"message":"not granted"}}"#),
            "not granted"
        );
        assert_eq!(broker_error_message("plain text"), "plain text");
    }

    #[test]
    fn new_trims_trailing_slashes() {
        let client = BrokerClient::new("http://127.0.0.1:3001/", "token");
        match client.transport {
            Transport::Http { base_url, .. } => assert_eq!(base_url, "http://127.0.0.1:3001"),
            Transport::Stdio => panic!("expected HTTP"),
        }
    }

    #[test]
    fn fs_calls_use_the_broker_endpoint() {
        let reply = r#"{"size":6,"modified":null,"is_dir":false}"#;
        let (base_url, server) = spawn_broker(reply.to_string());
        let client = BrokerClient::new(base_url, "run-token");
        let stat = client.fs_stat("file:///library/a.mp4").unwrap();
        assert_eq!(stat["size"], 6);
        let request = server.join().unwrap();
        assert!(
            request.contains("POST /api/v1/plugin-broker/fs"),
            "{request}"
        );
        assert!(request.contains("file:///library/a.mp4"), "{request}");
    }

    #[test]
    fn client_posts_the_token_and_decodes_the_reply() {
        let body = STANDARD.encode(b"{\"title\":\"demo\"}");
        let reply =
            format!(r#"{{"status":200,"content_type":"application/json","body_b64":"{body}"}}"#);
        let (base_url, server) = spawn_broker(reply);
        let client = BrokerClient::new(base_url, "run-token");
        let value: Value = client.fetch_json("https://api.example.com/x", &[]).unwrap();
        assert_eq!(value["title"], "demo");
        let request = server.join().unwrap();
        assert!(
            request.contains("POST /api/v1/plugin-broker/fetch"),
            "{request}"
        );
        assert!(
            request.contains("x-localreel-plugin: run-token"),
            "{request}"
        );
        assert!(request.contains("https://api.example.com/x"), "{request}");
    }
}
