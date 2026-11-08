//! Child for `appcontainer-probe`: prints whether one TCP target is reachable.
//!
//! Defaults to `1.1.1.1:80`; pass `host:port` to test loopback.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

fn main() {
    let target = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1.1.1.1:80".to_string());
    let address: SocketAddr = match target.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(address) => address,
        None => {
            println!("invalid target: {target}");
            std::process::exit(2);
        }
    };
    match TcpStream::connect_timeout(&address, Duration::from_secs(3)) {
        Ok(_) => {
            println!("connected");
            std::process::exit(1);
        }
        Err(error) => {
            println!("blocked: {error}");
            std::process::exit(0);
        }
    }
}
