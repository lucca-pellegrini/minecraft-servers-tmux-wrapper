// SPDX-License-Identifier: Apache-2.0

mod bluemap;
mod config;
mod minecraft_client;
mod socket;
mod tmux;

use std::{
    process::exit,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use log::{LevelFilter, debug, error, info, trace, warn};
use systemd_journal_logger::JournalLog;
use tokio::net::TcpStream;

use crate::config::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize the journald logger
    JournalLog::new().unwrap().install().unwrap();
    log::set_max_level(LevelFilter::Trace);

    // Create a vector for task handles.
    let mut tasks = Vec::new();

    // Get the TcpListeners from the systemd socket units
    let mut sockets = socket::get_systemd_sockets().await?;

    // Spawn a task to periodically check if the proxy is still online
    tokio::spawn(async {
        loop {
            // Check every 5 seconds
            tokio::time::sleep(Duration::from_secs(5)).await;

            // Only check the proxy if the servers have started
            if *SERVER_STATE.read().unwrap() == ServerState::Started {
                match TcpStream::connect(("127.0.0.1", PROXY_PORT)).await {
                    Ok(_) => continue,
                    Err(_) => {
                        warn!("Velocity proxy went offline");

                        // Clone the servers list before awaiting
                        let servers = SERVERS.lock().unwrap().clone();

                        match tmux::cleanup_servers(servers).await {
                            Ok(()) => {
                                info!("Exiting gracefully");
                                exit(0);
                            }
                            Err(e) => {
                                error!("Fatal error when cleaning up servers: {}", e);
                                exit(1);
                            }
                        }
                    }
                }
            }
        }
    });
    debug!("Spawned proxy verification task");

    // Initialize an atomic timestamp with the current time
    let last_connection_time = Arc::new(AtomicU64::new(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    ));

    // Task to monitor inactivity and exit if no connections are received
    let last_connection_time_clone = Arc::clone(&last_connection_time);
    tasks.push(tokio::spawn(async move {
        while *SERVER_STATE.read().unwrap() == ServerState::NotStarted {
            tokio::time::sleep(Duration::from_secs(1)).await;

            let last_time = last_connection_time_clone.load(Ordering::SeqCst);
            let current_time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();

            if current_time - last_time >= INIT_TIMEOUT {
                info!(
                    "No client connections received for {} seconds. Exiting.",
                    INIT_TIMEOUT
                );
                exit(0);
            }
        }
        debug!("Servers are starting. Stopping client connection watcher task.");
    }));
    debug!("Spawned client connection watcher task");

    if let Some(bm_listener) = sockets.remove(&BLUEMAP_PORT) {
        let last_connection_time_bm = last_connection_time.clone();
        debug!("Obtained TcpListener for BlueMap");

        tasks.push(tokio::spawn(async move {
            {
                let mut bm_webroot = BLUEMAP_WEBROOT.lock().unwrap();
                if let Some(dir) = bluemap::find_bluemap_dir() {
                    *bm_webroot = dir;
                } else {
                    error!("Could not find BlueMap Webroot. Connections will be ignored!");
                    return;
                }
            }

            while *SERVER_STATE.read().unwrap() < ServerState::ShuttingDown {
                match bm_listener.accept().await {
                    Ok((sock, addr)) => {
                        let id = crate::config::CONNECTION_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
                        let last_connection_time_clone = Arc::clone(&last_connection_time_bm);
                        let current_time = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();
                        last_connection_time_clone.store(current_time, Ordering::SeqCst);

                        trace!(
                            "[{:06X}]: Received BlueMap connection from {}. Storing last connection time: {:X}",
                            id, addr, current_time
                        );
                        tokio::spawn(async move {
                            if let Err(e) = bluemap::handle_connection(sock).await {
                                warn!("[{:06X}]: Bluemap handler error: {}", id, e);
                            }
                        });
                    }
                    Err(e) => {
                        warn!("Bluemap accept error: {}", e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }));
    } else {
        warn!(
            "Could not find a listener for the BlueMap port ({}). Disabling.",
            BLUEMAP_PORT
        );
    }

    // Handle client connections
    if let Some(mc_listener) = sockets.remove(&CLIENT_PORT) {
        debug!("Obtained TcpListener for Minecraft Clients");

        while *SERVER_STATE.read().unwrap() < ServerState::ShuttingDown {
            match mc_listener.accept().await {
                Ok((client, addr)) => {
                    let id = crate::config::CONNECTION_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
                    let last_connection_time_clone = Arc::clone(&last_connection_time);
                    let current_time = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    last_connection_time_clone.store(current_time, Ordering::SeqCst);

                    trace!(
                        "[{:06X}]: Finished handling connection from {} in {}s",
                        id,
                        addr,
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            - current_time
                    );

                    tasks.push(tokio::spawn(async move {
                        if let Err(e) = minecraft_client::handle_client(client, addr, id).await {
                            warn!("[{:06X}]: Error handling {}: {}", id, addr, e);
                        }
                    }));

                    trace!(
                        "[{:06X}]: Finished handling connection from {} in {}s",
                        id,
                        addr,
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs()
                            - current_time
                    );
                }
                Err(e) => {
                    warn!("Minecraft client accept error: {}", e);
                }
            }
        }
    } else {
        error!(
            "Systemd did not pass a file descriptor for the client port ({}). Panicking!",
            CLIENT_PORT
        );
        exit(1);
    }

    for task in tasks {
        trace!("Waiting for task {}", task.id());
        let _ = task.await;
    }

    Ok(())
}
