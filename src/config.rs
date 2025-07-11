// SPDX-License-Identifier: Apache-2.0

use atomic_enum::atomic_enum;
use std::sync::Mutex;

// Condiguration constants.
pub static SRV_DIR: &str = ".local/share/srv";
pub static SRV_STARTUP_SCRIPT: &str = "./run";
pub static TMUX_SESSION: &str = "fabric-servers";
pub static BUFFER_TIMEOUT: u64 = 90; // How long to await for proxy to start listening
pub static INIT_TIMEOUT: u64 = 60; // How long to wait for client connections before the servers start
pub static CLIENT_PORT: u16 = 25565; // Where systemd listens for minecraft client connections
pub static BLUEMAP_PORT: u16 = 31898; // Where systemd listens for bluemap http connections
pub static PROXY_PORT: u16 = 25564; // Port where the main Velocity proxy will listen
pub static BLUEMAP_MOD_PORT: u16 = 8100; // Port where the BlueMap mod will listen

// Global program state.
pub static SERVERS: Mutex<Vec<String>> = Mutex::new(Vec::new());
pub static SERVER_STATE: AtomicServerState = AtomicServerState::new(ServerState::NotStarted);
pub static BLUEMAP_WEBROOT: Mutex<String> = Mutex::new(String::new());

#[atomic_enum]
#[derive(PartialEq, PartialOrd, Eq, Ord)]
pub enum ServerState {
    NotStarted = 0,
    Starting,
    Started,
    ShuttingDown,
    ShutDown
}
