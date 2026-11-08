//! Test fixture: a stdio storage plugin whose stat/list/read go through the
//! host `fs.read` broker. It exists so the API integration test can exercise
//! the real grant + broker + storage-adapter chain without a direct filesystem
//! dependency.

use localreel_plugin::broker_client::BrokerClient;
use localreel_plugin::server::{self, Entry, Handler, HandlerError, Stat};
use serde_json::Value;

struct FsHandler {
    broker: BrokerClient,
}

impl Handler for FsHandler {
    fn stat(&self, uri: &str) -> Result<Stat, HandlerError> {
        let value = self.broker.fs_stat(uri).map_err(HandlerError::failed)?;
        Ok(Stat {
            size: value.get("size").and_then(Value::as_u64).unwrap_or(0),
            modified: value.get("modified").and_then(Value::as_i64),
            is_dir: value
                .get("is_dir")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn list(&self, uri: &str, recursive: bool) -> Result<Vec<Entry>, HandlerError> {
        let value = self.broker.fs_list(uri).map_err(HandlerError::failed)?;
        let items = value
            .get("entries")
            .and_then(Value::as_array)
            .ok_or_else(|| HandlerError::failed("fs.list response has no entries"))?;
        if recursive {
            return Err(HandlerError::failed("fixture does not walk"));
        }
        Ok(items
            .iter()
            .map(|item| Entry {
                uri: item
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                is_dir: item.get("is_dir").and_then(Value::as_bool).unwrap_or(false),
                size: item.get("size").and_then(Value::as_u64).unwrap_or(0),
                modified: item.get("modified").and_then(Value::as_i64),
            })
            .collect())
    }

    fn read(&self, uri: &str, start: u64, len: u64) -> Result<Vec<u8>, HandlerError> {
        self.broker
            .fs_read(uri, start, len)
            .map_err(HandlerError::failed)
    }
}

fn main() {
    let broker = BrokerClient::from_env().expect("fs broker is unavailable");
    if let Err(error) = server::run_stdio(Box::new(FsHandler { broker })) {
        eprintln!("fs fixture failed: {error}");
        std::process::exit(1);
    }
}
