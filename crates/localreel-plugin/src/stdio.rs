//! Newline-delimited JSON-RPC over a plugin's stdin/stdout.
//!
//! This is the non-TCP control channel: the plugin listens on nothing, and the
//! host talks to it through the pipes it already owns. One request is one line;
//! responses carry the same `id`. A reader thread routes responses so a call can
//! time out without blocking the whole runtime.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::manifest::PluginError;

const CALL_TIMEOUT: Duration = Duration::from_secs(60);

pub struct StdioTransport {
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
    next_id: AtomicU64,
    token: String,
}

impl StdioTransport {
    pub fn new(stdin: ChildStdin, stdout: ChildStdout, token: String) -> Self {
        let pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
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
            stdin: Mutex::new(stdin),
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
