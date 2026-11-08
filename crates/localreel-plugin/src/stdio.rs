//! Newline-delimited JSON-RPC over a plugin's stdin/stdout.
//!
//! This is the non-TCP control channel: the plugin listens on nothing, and the
//! host talks to it through the pipes it already owns. One request is one line;
//! responses carry the same `id`. A reader thread routes responses so a call can
//! time out without blocking the whole runtime.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::manifest::PluginError;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const BROKER_PREFIX: &str = "/api/v1/plugin-broker/";
const MAX_BROKER_BODY: usize = 32 * 1024 * 1024;

/// Host-side proxy used by stdio plugins: a broker request arrives on the
/// plugin's stdout, is forwarded to the host's own loopback API, and the reply
/// goes back on the plugin's stdin. The plugin never opens a socket.
#[derive(Clone)]
pub struct BrokerProxy {
    base_url: String,
    token: String,
    agent: ureq::Agent,
}

impl BrokerProxy {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let mut base_url = base_url.into();
        while base_url.ends_with('/') {
            base_url.pop();
        }
        Self {
            base_url,
            token: token.into(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .build(),
        }
    }

    fn forward(&self, method: &str, path: &str, body: &[u8]) -> Result<(u16, Vec<u8>), String> {
        if !path.starts_with(BROKER_PREFIX) {
            return Err("stdio broker only accepts /api/v1/plugin-broker/ paths".to_string());
        }
        let url = format!("{}{}", self.base_url, path);
        let mut request = self
            .agent
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
            Err(error) => return Err(format!("broker proxy failed: {error}")),
        };
        let status = response.status();
        Ok((status, read_body(response)))
    }
}

fn read_body(response: ureq::Response) -> Vec<u8> {
    let mut body = Vec::new();
    let _ = response
        .into_reader()
        .take(MAX_BROKER_BODY as u64)
        .read_to_end(&mut body);
    body
}

pub struct StdioTransport {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
    next_id: AtomicU64,
    token: String,
}

impl StdioTransport {
    pub fn new(
        stdin: ChildStdin,
        stdout: ChildStdout,
        token: String,
        broker: Option<BrokerProxy>,
    ) -> Self {
        let stdin = Arc::new(Mutex::new(stdin));
        let broker = broker.map(Arc::new);
        let pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let reader_stdin = Arc::clone(&stdin);
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(_) => break,
                };
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                if value.get("type").and_then(Value::as_str) == Some("broker.request") {
                    let id = value.get("id").and_then(Value::as_u64);
                    let method = value
                        .get("method")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let path = value
                        .get("path")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let body = value
                        .get("body_b64")
                        .and_then(Value::as_str)
                        .map(|encoded| {
                            base64::engine::general_purpose::STANDARD
                                .decode(encoded)
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    let (status, body) = match broker.as_ref() {
                        Some(proxy) => match proxy.forward(method, path, &body) {
                            Ok(reply) => reply,
                            Err(error) => (502, error.into_bytes()),
                        },
                        None => (503, b"stdio broker is not configured".to_vec()),
                    };
                    let reply = json!({
                        "type": "broker.reply",
                        "id": id,
                        "status": status,
                        "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
                    });
                    let mut stdin = reader_stdin
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let _ = writeln!(stdin, "{reply}");
                    let _ = stdin.flush();
                    continue;
                }
                let Some(id) = value.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let sender = reader_pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                if let Some(sender) = sender {
                    let _ = sender.send(value);
                }
            }
            reader_pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
        });
        Self {
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            token,
        }
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, PluginError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, sender);
        let line = json!({
            "id": id,
            "token": self.token,
            "method": method,
            "params": params,
        })
        .to_string();
        {
            let mut stdin = self
                .stdin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Err(error) = stdin
                .write_all(line.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
            {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                return Err(PluginError::Request(format!(
                    "cannot write to plugin stdin: {error}"
                )));
            }
        }

        let response = match receiver.recv_timeout(CALL_TIMEOUT) {
            Ok(response) => response,
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                return Err(PluginError::Request(
                    "stdio plugin did not answer in time".to_string(),
                ));
            }
        };
        if response.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(response.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(PluginError::Request(
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("plugin returned an error")
                    .to_string(),
            ))
        }
    }

    pub fn health(&self) -> Result<(), PluginError> {
        self.call("health", json!({})).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;

    fn fixture() -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
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
            let body = r#"{"status":200,"body_b64":"aGk="}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            request
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn proxy_forwards_only_broker_paths() {
        let proxy = BrokerProxy::new("http://127.0.0.1:9", "token");
        assert!(proxy.forward("GET", "/api/v1/admin/plugins", &[]).is_err());
    }

    #[test]
    fn proxy_forwards_a_broker_call() {
        let (base_url, server) = fixture();
        let proxy = BrokerProxy::new(base_url, "run-token");
        let (status, body) = proxy
            .forward(
                "POST",
                "/api/v1/plugin-broker/fetch",
                br#"{"url":"https://example.com"}"#,
            )
            .unwrap();
        assert_eq!(status, 200);
        assert!(!body.is_empty());
        let request = server.join().unwrap();
        assert!(request.contains("POST /api/v1/plugin-broker/fetch"));
        assert!(request.contains("x-localreel-plugin: run-token"));
        assert!(request
            .to_lowercase()
            .contains("content-type: application/json"));
        assert!(request.contains("https://example.com"));
    }
}
