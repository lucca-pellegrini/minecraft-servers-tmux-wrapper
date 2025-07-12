// SPDX-License-Identifier: Apache-2.0

mod config;
mod connect;
mod tmux;

use std::{
    os::unix::io::FromRawFd,
    process::exit,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use log::{LevelFilter, debug, info, trace, warn};
use systemd_journal_logger::JournalLog;
use tokio::net::{TcpListener, TcpStream};

use crate::config::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize the journald logger
    JournalLog::new().unwrap().install().unwrap();
    log::set_max_level(LevelFilter::Trace);

    // Get TcpListener from the systemd socket unit
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;
    debug!("Initialized TcpListener");

    // Spawn a task to periodically check if the proxy is still online
    tokio::spawn(async {
        loop {
            // Check every 5 seconds
            tokio::time::sleep(Duration::from_secs(5)).await;
            trace!("Checking if proxy is still running");

            // Only check the proxy if the servers have started
            if SERVER_STATE.load(Ordering::SeqCst) == ServerState::Started {
                match TcpStream::connect(("127.0.0.1", PROXY_PORT)).await {
                    Ok(_) => {
                        trace!("Proxy is still online");
                        continue;
                    }
                    Err(_) => {
                        warn!("Velocity proxy went offline");

                        // Clone the servers list before awaiting
                        let servers = SERVERS.lock().unwrap().clone();

                        tmux::cleanup_servers(servers).await;
                        info!("Exiting gracefully");
                        exit(0);
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
    tokio::spawn(async move {
        while SERVER_STATE.load(Ordering::SeqCst) == ServerState::NotStarted {
            tokio::time::sleep(Duration::from_secs(1)).await;
            trace!("Checking for client connections.");

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
            } else {
                trace!(
                    "Timeout for client connections ({}s) not reached. Currently at {}s",
                    INIT_TIMEOUT,
                    current_time - last_time
                );
            }
        }
        debug!("Servers are starting. Stopping client connection watcher task.");
    });
    debug!("Spawned client connection watcher task");

    // Handle client connections
    loop {
        let (client, addr) = listener.accept().await?;
        let last_connection_time_clone = Arc::clone(&last_connection_time);
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        last_connection_time_clone.store(current_time, Ordering::SeqCst);

        trace!(
            "Received client connection from {}. Storing last connection time: {}",
            addr, current_time
        );

        tokio::spawn(async move {
            if let Err(e) = connect::handle_client(client, addr).await {
                warn!("Error handling {}: {}", addr, e);
            }
        });
    }
}
