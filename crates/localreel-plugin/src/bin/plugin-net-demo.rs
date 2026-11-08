//! Stdio plugin used by the AppContainer test: tries one outbound TCP connect
//! and reports the result on stderr (stdout is the protocol channel).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use localreel_plugin::server::{self, Entry, Handler, HandlerError, Stat};

struct NetHandler;

impl Handler for NetHandler {
    fn stat(&self, _uri: &str) -> Result<Stat, HandlerError> {
        Err(HandlerError::NotFound)
    }

    fn list(&self, _uri: &str, _recursive: bool) -> Result<Vec<Entry>, HandlerError> {
        Err(HandlerError::NotFound)
    }

    fn read(&self, _uri: &str, _start: u64, _len: u64) -> Result<Vec<u8>, HandlerError> {
        Err(HandlerError::NotFound)
    }

    fn schedule(&self, _payload: &str) -> Result<(), HandlerError> {
        let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 80);
        match TcpStream::connect_timeout(&target, Duration::from_secs(3)) {
            Ok(_) => eprintln!("net: connected"),
            Err(_) => eprintln!("net: blocked"),
        }
        Ok(())
    }
}

fn main() {
    if let Err(error) = server::run_stdio(Box::new(NetHandler)) {
        eprintln!("net demo failed: {error}");
        std::process::exit(1);
    }
}
