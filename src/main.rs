use std::{
    env, fs,
    os::unix::io::FromRawFd,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
};

use regex::Regex;

static SRV_DIR: &str = ".local/share/srv";
static SRV_STARTUP_SCRIPT: &str = "./run";
static TMUX_SESSION: &str = "fabric-servers";
static PROXY_PORT: u16 = 25564;
static BUFFER_TIMEOUT: u64 = 25;

fn tmux_socket_path() -> PathBuf {
    let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
    PathBuf::from(runtime).join("fabric-servers.sock")
}

fn start_tmux() {
    let tmux_socket = tmux_socket_path();

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

    let server_root = dirs::home_dir().unwrap().join(SRV_DIR);

    let mut servers: Vec<_> = fs::read_dir(&server_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .collect();

    servers.sort_by_key(|e| e.file_name());

    if servers.is_empty() {
        eprintln!("No servers found.");
        return;
    }

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

    for srv in servers {
        let srv_path = srv.path();
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
    }
}

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

async fn handle_client(mut client: TcpStream) -> std::io::Result<()> {
    let mut proxy = TcpStream::connect(("127.0.0.1", PROXY_PORT)).await?;
    let _ = copy_bidirectional(&mut client, &mut proxy).await;
    Ok(())
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
    std_listener.set_nonblocking(true)?;
    let listener = TcpListener::from_std(std_listener)?;

    start_tmux();

    if !wait_for_proxy().await {
        eprintln!("Velocity proxy did not start in time");
        std::process::exit(1);
    }

    loop {
        let (client, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = handle_client(client).await {
                eprintln!("Error forwarding: {}", e);
            }
        });
    }
}
