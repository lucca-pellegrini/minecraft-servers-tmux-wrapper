// SPDX-License-Identifier: Apache-2.0

use std::{
    env, fs, io,
    path::PathBuf,
    process::{Command, Stdio, exit},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use log::{debug, error, info, trace, warn};
use nix::{
    sys::signal::{
        Signal::{SIGHUP, SIGINT, SIGKILL, SIGTERM},
        kill,
    },
    unistd::Pid,
};

use regex::Regex;

use crate::SERVER_STATE;
use crate::SERVERS;
use crate::ServerState;
use crate::config::*;

// Start all tmux windows and update the SERVERS list
pub fn start_servers() -> anyhow::Result<()> {
    match start_tmux_windows() {
        Ok(s) => {
            trace!("Tmux windows started successfully");

            // Extend the SERVERS list with the started servers
            SERVERS.lock().unwrap().extend(s);
            trace!("Extended servers list with the started servers");

            Ok(())
        }
        Err(e) => {
            error!("Failed to start the tmux servers: {}", e);
            exit(1);
        }
    }
}

// Attempt to clean up servers gracefully
pub async fn cleanup_servers(servers: Vec<String>) -> () {
    trace!("Cleaning up servers");

    let tmux_socket = get_tmux_socket_path();

    // Determine the PID of the main process in each pane's main window
    let server_pids: Vec<Pid> = servers
        .iter()
        .map(|window| {
            let output = Command::new("tmux")
                .args([
                    "-S",
                    tmux_socket.to_str().unwrap(),
                    "display-message",
                    "-pt",
                    &format!("{}:{}.0", TMUX_SESSION, window), // Assuming single-pane windows
                    "#{pane_pid}",
                ])
                .output()
                .expect(&format!("Failed to query PID for server: {}", window));

            trace!(
                "Got PID {} for server {}",
                String::from_utf8_lossy(&output.stdout),
                window
            );

            Pid::from_raw(
                String::from_utf8_lossy(&output.stdout)
                    .to_string()
                    .trim()
                    .parse::<i32>()
                    .expect(&format!("Failed to parse PID for server: {}", window)),
            )
        })
        .collect();

    let mut tasks = Vec::new(); // Create a Vec to store JoinHandles

    // Clean up servers asynchronously
    for pid in server_pids {
        let handle = tokio::spawn(async move {
            // Try each signal up to three times
            for signal in [SIGTERM, SIGINT, SIGHUP] {
                for _ in 0..3 {
                    if !is_process_running(pid) {
                        debug!("Process {} terminated gracefully.", pid);
                        return;
                    }

                    debug!("Sending {} to process {}.", signal.as_str(), pid);
                    let _ = kill(pid, signal);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }

            // Finally, send SIGKILL
            if is_process_running(pid) {
                warn!("Sending SIGKILL to process {}.", pid);
                let _ = kill(pid, SIGKILL);
            }
        });

        trace!("Spawned task {} to clean up process {}", handle.id(), pid);
        tasks.push(handle); // Store the JoinHandle
    }

    // Await all tasks
    for task in tasks {
        trace!("Waiting for cleanup task {}", task.id());
        let _ = task.await;
    }

    info!("Cleaned up all servers");
}

// Initialize proxy and servers
fn start_tmux_windows() -> anyhow::Result<Vec<String>, io::Error> {
    trace!("Starting tmux windows");

    if SERVER_STATE.load(Ordering::SeqCst) != ServerState::NotStarted {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Startup task is already running.",
        ));
    }

    SERVER_STATE.store(ServerState::Starting, Ordering::SeqCst);
    trace!("Stored server state");

    // To make cheap copies for asynchronous use
    let tmux_socket = Arc::new(get_tmux_socket_path());

    // Kill any existing session
    let _ = Command::new("tmux")
        .args([
            "-S",
            tmux_socket.to_str().unwrap(),
            "kill-session",
            "-t",
            TMUX_SESSION,
        ])
        .stderr(Stdio::null())
        .output();

    // Determine the server's root directory
    let server_root = dirs::home_dir().unwrap().join(SRV_DIR);
    trace!(
        "Determined servers root at {}",
        server_root.to_str().unwrap()
    );

    // Read directory to get server names
    let mut servers: Vec<_> = fs::read_dir(&server_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect();

    // Raise an error if there are no servers to start
    if servers.is_empty() {
        error!("No servers found.");
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Found no servers in the directory",
        ));
    }

    // Determine the names of the windows from the server paths
    let window_names = servers
        .iter()
        .filter_map(|server| Some(server.file_name().to_string_lossy().into_owned()))
        .collect();

    // Create a case-insensitive regex to match '.*Velocity.*'
    let re = Regex::new(".*Velocity.*").unwrap();

    // Find the index of the first server matching the regex
    let index = servers
        .iter()
        .position(|e| re.is_match(&e.file_name().to_string_lossy()));

    let first = if let Some(i) = index {
        debug!(
            "Determined proxy server to be {}",
            servers[i].file_name().to_str().unwrap()
        );
        servers.remove(i) // Remove the matching server
    } else {
        warn!("Could not determine proxy server. Carrying on as if nothing were amiss...");
        servers.remove(0) // Remove the first server if no match found
    };
    let first_path = first.path();

    // Spawn the session, starting the first server
    let _ = Command::new("tmux")
        .args([
            "-S",
            tmux_socket.to_str().unwrap(),
            "new-session",
            "-d",
            "-s",
            TMUX_SESSION,
            "-n",
            first.file_name().to_str().unwrap(),
            "-c",
            first_path.to_str().unwrap(),
            SRV_STARTUP_SCRIPT,
        ])
        .output();
    trace!(
        "Spawned main tmux session for server {}",
        first_path.to_str().unwrap()
    );

    // For all remaining servers, start them asynchronously
    for srv in servers {
        // Cheap Arc copy, for ownership shenanigans.
        let tmux_socket = tmux_socket.clone();
        let srv_path = srv.path();

        trace!(
            "Spawning tmux window for server {}",
            srv.file_name().to_str().unwrap()
        );

        tokio::spawn(async move {
            let _ = Command::new("tmux")
                .args([
                    "-S",
                    tmux_socket.to_str().unwrap(),
                    "new-window",
                    "-t",
                    TMUX_SESSION,
                    "-n",
                    srv.file_name().to_str().unwrap(),
                    "-c",
                    srv_path.to_str().unwrap(),
                    SRV_STARTUP_SCRIPT,
                ])
                .output();
        });
    }

    // Return the window names
    Ok(window_names)
}

// Determine path to the tmux socket
fn get_tmux_socket_path() -> PathBuf {
    let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
    PathBuf::from(runtime).join("fabric-servers.sock")
}

// Check if a process is still running by calling `kill -0` on its PID
fn is_process_running(pid: Pid) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
