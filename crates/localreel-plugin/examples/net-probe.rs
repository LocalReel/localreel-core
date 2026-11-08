//! Child for `appcontainer-probe`: exits 0 only when outbound TCP is denied.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

fn main() {
    let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 80);
    match TcpStream::connect_timeout(&target, Duration::from_secs(3)) {
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
