//! Plugin-side client for the host's capability broker.
//!
//! A plugin should not grow its own HTTP policy. This client posts to the host
//! broker with the per-run token and decodes the base64 body; host-side grants,
//! scope checks, SSRF policy and audit stay in one place.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use base64::Engine as _;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

/// One decoded broker answer.
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

/// A small blocking client. Clone cheaply: the `ureq::Agent` pools internally.
#[derive(Clone)]
pub struct BrokerClient {
    base_url: String,
    token: String,
    agent: ureq::Agent,
}

impl BrokerClient {
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

    /// Read `LOCALREEL_PLUGIN_HOST_URL` and `LOCALREEL_PLUGIN_TOKEN`.
    ///
    /// Returns `None` when the host did not provide a broker URL, which is the
    /// documented phase-1 behaviour.
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("LOCALREEL_PLUGIN_HOST_URL").ok()?;
        let token = std::env::var("LOCALREEL_PLUGIN_TOKEN").ok()?;
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
            payload["secret"] = serde_json::Value::String(handle.to_string());
        }
        let endpoint = format!("{}/api/v1/plugin-broker/fetch", self.base_url);
        let response = match self
            .agent
            .post(&endpoint)
            .set("x-localreel-plugin", &self.token)
            .set("content-type", "application/json")
            .send_string(&payload.to_string())
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let mut text = String::new();
                let _ = response
                    .into_reader()
                    .take(8 * 1024)
                    .read_to_string(&mut text);
                return Err(format!(
                    "broker refused the call ({status}): {}",
                    broker_error_message(&text)
                ));
            }
            Err(error) => return Err(format!("broker request failed: {error}")),
        };
        let mut text = String::new();
        response
            .into_reader()
            .take(32 * 1024 * 1024)
            .read_to_string(&mut text)
            .map_err(|error| format!("broker response could not be read: {error}"))?;
        parse_reply(&text)
    }

    /// Ask the host for a periodic callback (`GET /v1/schedule`).
    pub fn request_schedule(&self, interval_secs: u64, payload: &Value) -> Result<Value, String> {
        let body = serde_json::json!({
            "interval_secs": interval_secs,
            "payload": payload,
        });
        let endpoint = format!("{}/api/v1/plugin-broker/schedule", self.base_url);
        let response = match self
            .agent
            .post(&endpoint)
            .set("x-localreel-plugin", &self.token)
            .set("content-type", "application/json")
            .send_string(&body.to_string())
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let mut text = String::new();
                let _ = response
                    .into_reader()
                    .take(8 * 1024)
                    .read_to_string(&mut text);
                return Err(format!(
                    "schedule.request refused the call ({status}): {}",
                    broker_error_message(&text)
                ));
            }
            Err(error) => return Err(format!("schedule.request failed: {error}")),
        };
        let mut text = String::new();
        response
            .into_reader()
            .take(1024 * 1024)
            .read_to_string(&mut text)
            .map_err(|error| format!("schedule.response could not be read: {error}"))?;
        serde_json::from_str(&text)
            .map_err(|error| format!("schedule.response is not JSON: {error}"))
    }

    pub fn fetch_json<T: DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<T, String> {
        self.fetch(url, headers)?.json()
    }

    /// Stage normalized titles through `catalog.write(draft)`.
    pub fn write_catalog<T: Serialize>(&self, items: &[T]) -> Result<usize, String> {
        let payload = serde_json::json!({ "items": items });
        let endpoint = format!("{}/api/v1/plugin-broker/catalog", self.base_url);
        let response = match self
            .agent
            .post(&endpoint)
            .set("x-localreel-plugin", &self.token)
            .set("content-type", "application/json")
            .send_string(&payload.to_string())
        {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let mut text = String::new();
                let _ = response
                    .into_reader()
                    .take(8 * 1024)
                    .read_to_string(&mut text);
                return Err(format!(
                    "catalog.write refused the batch ({status}): {}",
                    broker_error_message(&text)
                ));
            }
            Err(error) => return Err(format!("catalog.write request failed: {error}")),
        };
        let mut text = String::new();
        response
            .into_reader()
            .take(8 * 1024 * 1024)
            .read_to_string(&mut text)
            .map_err(|error| format!("catalog.write response could not be read: {error}"))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("catalog.write response is not JSON: {error}"))?;
        value
            .get("written")
            .and_then(Value::as_u64)
            .and_then(|written| usize::try_from(written).ok())
            .ok_or_else(|| "catalog.write response has no written count".to_string())
    }
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
        assert_eq!(client.base_url, "http://127.0.0.1:3001");
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
