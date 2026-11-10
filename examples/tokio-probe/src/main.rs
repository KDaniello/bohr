//! The smallest program that starts a `tokio` reactor and asks it to do something.
//!
//! For surveys: Rust network clients are built on this runtime's reactor.
//!
//! A current-thread runtime, so the calls are the reactor's.

use std::{net::SocketAddrV4, time::Duration};

use tokio::net::UdpSocket;

fn main() {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cannot build a runtime: {e}");
            std::process::exit(2);
        }
    };

    let code = runtime.block_on(async {
        // A timer, which needs the driver, and a socket, which needs the reactor.
        tokio::time::sleep(Duration::from_millis(1)).await;

        let socket = match UdpSocket::bind(SocketAddrV4::new([10, 66, 0, 1].into(), 7000)).await {
            Ok(socket) => socket,
            Err(e) => {
                eprintln!("cannot bind: {e}");
                return 2;
            }
        };
        let peer = SocketAddrV4::new([10, 66, 0, 1].into(), 7000);
        if socket.send_to(b"probe", peer).await.is_err() {
            eprintln!("cannot send");
            return 2;
        }
        let mut buf = [0u8; 32];
        match socket.recv_from(&mut buf).await {
            Ok((len, _)) => {
                println!("read back {len} bytes");
                0
            }
            Err(e) => {
                eprintln!("cannot receive: {e}");
                2
            }
        }
    });
    std::process::exit(code);
}
