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
    time::sleep,
};

static SRV_DIR: &str = ".local/share/srv";
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

    let first = servers.remove(0);
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
            "./run",
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
                "./run",
            ])
            .output();
    }
}

async fn wait_for_proxy() -> bool {
    for _ in 0..(BUFFER_TIMEOUT * 10) {
        match TcpStream::connect(("127.0.0.1", PROXY_PORT)).await {
            Ok(_) => return true,
            Err(_) => sleep(Duration::from_millis(100)).await,
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
    let listener = TcpListener::from_std(unsafe { std::net::TcpListener::from_raw_fd(3) })?;

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
