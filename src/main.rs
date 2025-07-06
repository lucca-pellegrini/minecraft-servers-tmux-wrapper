use std::{
    env, fs, io,
    os::unix::io::FromRawFd,
    path::PathBuf,
    process::{Command, Stdio, exit},
    sync::Arc,
    time::Duration,
};

use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
};

use nix::{
    sys::signal::{
        Signal::{SIGHUP, SIGINT, SIGKILL, SIGTERM},
        kill,
    },
    unistd::Pid,
};

use regex::Regex;

static SRV_DIR: &str = ".local/share/srv";
static SRV_STARTUP_SCRIPT: &str = "./run";
static TMUX_SESSION: &str = "fabric-servers";
static PROXY_PORT: u16 = 25564;
static BUFFER_TIMEOUT: u64 = 25;

// Determine path to the tmux socket
fn get_tmux_socket_path() -> PathBuf {
    let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
    PathBuf::from(runtime).join("fabric-servers.sock")
}

// Initialize proxy and servers
fn start_tmux_windows() -> Result<Vec<String>, std::io::Error> {
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

    // Read directory to get server names
    let mut servers: Vec<_> = fs::read_dir(&server_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect();

    // Raise an error if there are no servers to start
    if servers.is_empty() {
        eprintln!("No servers found.");
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
        servers.remove(i) // Remove the matching server
    } else {
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

    // For all remaining servers, start them asynchronously
    for srv in servers {
        // Cheap Arc copy, for ownership shenanigans.
        let tmux_socket = tmux_socket.clone();
        let srv_path = srv.path();

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

// Wait for proxy to start accepting connections
async fn wait_for_proxy() -> bool {
    let start = tokio::time::Instant::now();
    let timeout = Duration::from_secs(BUFFER_TIMEOUT);

    loop {
        if start.elapsed() >= timeout {
            break;
        }

        match TcpStream::connect(("127.0.0.1", PROXY_PORT)).await {
            Ok(_) => return true,
            Err(_) => tokio::task::yield_now().await, // Yield to allow other tasks to run
        }
    }
    false
}

// Handle an individual client connection by copying data bidirectionally
async fn handle_client(mut client: TcpStream) -> std::io::Result<()> {
    let mut proxy = TcpStream::connect(("127.0.0.1", PROXY_PORT)).await?;
    let _ = copy_bidirectional(&mut client, &mut proxy).await;
    Ok(())
}

// Attempt to clean up servers gracefully
async fn cleanup_servers(servers: Vec<String>) -> () {
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
                        eprintln!("Process {} terminated gracefully.", pid);
                        return;
                    }

                    eprintln!("Sending {} to process {}.", signal.as_str(), pid);
                    let _ = kill(pid, signal);
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }

            // Finally, send SIGKILL
            if is_process_running(pid) {
                eprintln!("Sending SIGKILL to process {}.", pid);
                let _ = kill(pid, SIGKILL);
            }
        });

        tasks.push(handle); // Store the JoinHandle
    }

    // Await all tasks
    for task in tasks {
        let _ = task.await;
    }
}

// Check if a process is still running by calling `kill -0` on it's PID
fn is_process_running(pid: Pid) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Get TcpListener from the systemd socket unit
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;

    // Start all servers
    let servers = match start_tmux_windows() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Failed to start the tmux servers: {}", e);
            exit(1);
        }
    };

    // Wait for proxy to accept connections (up to a timeout)
    if !wait_for_proxy().await {
        eprintln!("Velocity proxy did not start in time");
        exit(1);
    }

    // Spawn a task to periodically check if the proxy is still online
    tokio::spawn(async {
        loop {
            // Check every 5 seconds
            tokio::time::sleep(Duration::from_secs(5)).await;

            match TcpStream::connect(("127.0.0.1", PROXY_PORT)).await {
                Ok(_) => continue, // Proxy is still online
                Err(_) => {
                    eprintln!("Velocity proxy went offline");
                    cleanup_servers(servers).await;
                    eprintln!("Exiting gracefully");
                    exit(0);
                }
            }
        }
    });

    // Handle client connections
    loop {
        let (client, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = handle_client(client).await {
                eprintln!("Error forwarding: {}", e);
            }
        });
    }
}
