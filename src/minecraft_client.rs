// SPDX-License-Identifier: Apache-2.0

use std::{io, net::SocketAddr, process::exit, sync::atomic::Ordering, time::Duration};

use base64::{Engine, prelude::BASE64_STANDARD};
use log::{debug, error, info, trace};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::TcpStream,
};

use valence_protocol::{
    PacketDecoder, PacketEncoder,
    packets::{
        handshaking::handshake_c2s::{HandshakeC2s, HandshakeNextState},
        status::QueryResponseS2c,
    },
};

use bytes::BytesMut;
use serde_json::{Value, json};

use crate::PROXY_PORT;
use crate::SERVER_STATE;
use crate::ServerState;
use crate::{BUFFER_TIMEOUT, tmux};

// Handle an individual client connection by copying data bidirectionally
pub async fn handle_client(mut client: TcpStream, addr: SocketAddr, id: u64) -> anyhow::Result<()> {
    trace!("[{:06}]: Handling client from {}", id, addr);

    match SERVER_STATE.load(Ordering::SeqCst) {
        ServerState::ShuttingDown | ServerState::ShutDown => {
            debug!(
                "[{:06}]: Server is shutting down. Dropping connection from {}",
                id, addr
            );
            Ok(())
        }

        ServerState::Started => {
            trace!(
                "[{:06}]: Server already started, passing connection from {} to proxy",
                id, addr
            );
            return pass_connection(&mut client).await;
        }

        ServerState::Starting => {
            trace!(
                "[{:06}]: Server starting, waiting to pass connection from {}",
                id, addr
            );
            wait_for_proxy().await.unwrap_or_else(|e| {
                error!(
                    "[{:06}]: Failed while waiting for server to start to pass connection from {}: {}",
                    id, addr, e
                );
                exit(1);
            });
            trace!(
                "[{:06}]: Server successfully started, passing connection from {} to proxy",
                id, addr
            );
            return pass_connection(&mut client).await;
        }

        ServerState::NotStarted => {
            trace!(
                "[{:06}]: Server not started, checking packets from {}",
                id, addr
            );

            // Create a buffer to read the packet
            let mut buf = BytesMut::with_capacity(4096);
            let n = client.read_buf(&mut buf).await?;
            buf.truncate(n);

            // Create a PacketDecoder
            let mut decoder = PacketDecoder::new();
            decoder.queue_bytes(buf.clone()); // Clone buf to keep the original data

            // Decode the Handshake packet
            if let Ok(Some(frame)) = decoder.try_next_packet() {
                trace!("[{:06}]: Decoded minecraft packet from {}", id, addr);

                if let Ok(handshake) = frame.decode::<HandshakeC2s>() {
                    trace!("[{:06}]: Packet from {} is a C2S handshake", id, addr);
                    return handle_handshake(handshake, buf, client, addr, id).await;
                } else {
                    trace!(
                        "[{:06}]: Packet from {} is not a handshake packet. Ignoring",
                        id, addr
                    );
                }
            } else {
                trace!("[{:06}]: No minecraft packet from {}. Ignoring", id, addr);
            }

            Ok(())
        }
    }
}

// Respond to a handshake.
async fn handle_handshake(
    handshake: HandshakeC2s<'_>,
    packet_bytes: BytesMut,
    mut client: TcpStream,
    addr: SocketAddr,
    id: u64,
) -> anyhow::Result<()> {
    match handshake.next_state {
        // If it's a login attempt, start the servers and pass the connection, including the
        // original packet.
        HandshakeNextState::Login => {
            info!(
                "[{:06}]: Starting servers due to login attempt from {}",
                id, addr
            );
            tmux::start_servers()?;

            // Wait for the proxy to start
            wait_for_proxy().await.unwrap_or_else(|e| {
                 error!(
                     "[{:06}]: Failed while waiting for server to start to pass connection from {}: {}",
                     id, addr, e
                 );
                 exit(1);
             });
            info!("[{:06}]: Servers started for {}", id, addr);

            // Connect to the proxy
            let mut proxy = TcpStream::connect(("127.0.0.1", PROXY_PORT)).await?;

            // Send the original handshake packet to the proxy
            trace!(
                "[{:06}]: Passing original handshake packet from {} to proxy",
                id, addr
            );
            proxy.write_all(&packet_bytes).await?;

            // Start bidirectional data transfer
            trace!("[{:06}]: Passing connection from {} to proxy", id, addr);
            copy_bidirectional(&mut client, &mut proxy).await?;
            return Ok(());
        }

        // If it's a status request, respond with a valid JSON response.
        // See <https://minecraft.wiki/w/Java_Edition_protocol/Server_List_Ping?oldid=3034438>
        HandshakeNextState::Status => {
            trace!("[{:06}]: Packet from {} is a status request", id, addr);

            // Construct status response JSON
            let mut favicon_buf = "data:image/png;base64,".to_owned();
            BASE64_STANDARD.encode_string(include_bytes!("favicon.png"), &mut favicon_buf);

            let description: Value = serde_json::from_str(include_str!("server_description.json"))?;

            let json = json!({
                "version": {
                    "name": "1.21.1",
                    "protocol": 763,
                },
                "description": description,
                "favicon": Value::String(favicon_buf),
            });

            // Respond to the status packet.
            let mut encoder = PacketEncoder::new();
            let pkt = QueryResponseS2c {
                json: &json.to_string(),
            };
            encoder.append_packet(&pkt)?;

            trace!("[{:06}]: Sending status response to {}", id, addr);
            client.write_all(&encoder.take()).await?;

            Ok(())
        }
    }
}

// Wait for proxy to start accepting connections
async fn wait_for_proxy() -> anyhow::Result<(), io::Error> {
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(BUFFER_TIMEOUT);

    loop {
        if start.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Timed out while waiting for proxy to start.",
            ));
        }

        match TcpStream::connect(("127.0.0.1", PROXY_PORT)).await {
            Ok(_) => {
                SERVER_STATE.store(ServerState::Started, Ordering::SeqCst);
                return Ok(());
            }
            Err(_) => tokio::task::yield_now().await, // Yield to allow other tasks to run
        }
    }
}

// Pass the client connection to the proxy and copy data bidirectionally
async fn pass_connection(client: &mut TcpStream) -> anyhow::Result<()> {
    let mut proxy = TcpStream::connect(("127.0.0.1", PROXY_PORT)).await?;
    copy_bidirectional(client, &mut proxy).await?;
    Ok(())
}
