//! Minimal stdio plugin used by the runtime test: its `schedule` handler asks
//! the host broker for a URL over the inherited pipe and writes the body to
//! `broker-result.txt` in the plugin working directory.

use localreel_plugin::broker_client::BrokerClient;
use localreel_plugin::server::{self, Handler, HandlerError};

struct BrokerHandler;

impl Handler for BrokerHandler {
    fn stat(&self, _uri: &str) -> Result<localreel_plugin::server::Stat, HandlerError> {
        Err(HandlerError::NotFound)
    }

    fn list(
        &self,
        _uri: &str,
        _recursive: bool,
    ) -> Result<Vec<localreel_plugin::server::Entry>, HandlerError> {
        Err(HandlerError::NotFound)
    }

    fn read(&self, _uri: &str, _start: u64, _len: u64) -> Result<Vec<u8>, HandlerError> {
        Err(HandlerError::NotFound)
    }

    fn schedule(&self, _payload: &str) -> Result<(), HandlerError> {
        let client =
            BrokerClient::from_env().ok_or_else(|| HandlerError::Failed("no broker".into()))?;
        let reply = client
            .fetch("https://example.com/data", &[])
            .map_err(HandlerError::Failed)?;
        std::fs::write("broker-result.txt", &reply.body)
            .map_err(|error| HandlerError::Failed(error.to_string()))?;
        Ok(())
    }
}

fn main() {
    if let Err(error) = server::run_stdio(Box::new(BrokerHandler)) {
        eprintln!("broker demo failed: {error}");
        std::process::exit(1);
    }
}
