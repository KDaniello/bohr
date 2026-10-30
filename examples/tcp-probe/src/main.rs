//! The smallest program that carries a request over a stream socket and reads the answer back.
//!
//! It exists to be surveyed rather than to be hunted. Every component of an OP Stack node talks
//! over a stream: the engine API between the consensus client and the execution client, the JSON-RPC
//! a user calls, the peer-to-peer transport, the batcher's calls to the settlement layer. A model
//! that answers datagrams and nothing else reaches none of them, so the set this probe needs is the
//! set that stands between the engine and every one of those programs.
//!
//! Both ends live in one process on purpose: a listener and a caller are two halves of one
//! conversation, and putting them on separate nodes would measure the network model as well as the
//! socket family, when only the second is in question here.

use std::{net::SocketAddrV4, process::ExitCode};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

/// The port a consensus client calls its execution client on. Nothing here depends on the number;
/// it is the real one so a reader recognises the shape.
const ENGINE_PORT: u16 = 8551;

/// One request, in the shape the engine API uses, kept to a single line so no framing is needed.
const REQUEST: &[u8] = br#"{"jsonrpc":"2.0","method":"engine_getPayloadV3","id":1}"#;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cannot build a runtime: {e}");
            return ExitCode::from(2);
        }
    };
    runtime.block_on(converse())
}

/// Binds, accepts, sends a request, answers it and reads the answer back.
async fn converse() -> ExitCode {
    let address = SocketAddrV4::new([10, 66, 0, 1].into(), ENGINE_PORT);

    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("cannot listen: {e}");
            return ExitCode::from(2);
        }
    };

    // The caller is spawned rather than awaited in turn, because a connection is not established
    // until both ends have acted: awaiting the connect before the accept would wedge the probe on
    // a single-threaded runtime whatever the model underneath does.
    let caller = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await?;
        stream.write_all(REQUEST).await?;
        let mut answer = [0u8; 64];
        let read = stream.read(&mut answer).await?;
        Ok::<usize, std::io::Error>(read)
    });

    let (mut accepted, peer) = match listener.accept().await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("cannot accept: {e}");
            return ExitCode::from(2);
        }
    };

    let mut request = [0u8; 128];
    let asked = match accepted.read(&mut request).await {
        Ok(asked) => asked,
        Err(e) => {
            eprintln!("cannot read the request: {e}");
            return ExitCode::from(2);
        }
    };
    if accepted.write_all(br#"{"jsonrpc":"2.0","result":null,"id":1}"#).await.is_err() {
        eprintln!("cannot answer");
        return ExitCode::from(2);
    }

    match caller.await {
        Ok(Ok(answered)) => {
            println!("from {peer}: asked {asked} bytes, answered {answered}");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            eprintln!("the caller failed: {e}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("the caller did not finish: {e}");
            ExitCode::from(2)
        }
    }
}
