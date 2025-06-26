# fabric-servers-tmux-wrapper

A systemd socket-activated daemon for launching a network of Minecraft
(“Fabric”) servers in separate tmux windows behind a single Velocity proxy.
When the proxy (Velocity) goes down, the daemon attempts a graceful shutdown of
all game servers, escalating to SIGKILL if necessary.

---

## Features

- Socket-activated via systemd (`.socket` + `.service` units)
- Lazily loads each server directory under `~/.local/share/srv/`
- One tmux session (`fabric-servers`) with one window per server
- Proxy-required architecture: all game-server traffic is forwarded through
  Velocity on port 25564
- Graceful and forcible cleanup of server processes if the proxy dies
- Built on Rust, Tokio, nix (signals), regex, dirs

---

## Requirements

- Linux with systemd (≥ v229)
- tmux
- Rust toolchain (stable)
- Minecraft servers in `~/.local/share/srv/<server-name>/run`
- Velocity proxy listening on port `25564`
- (Optional) [lazymc](https://github.com/timvisee/lazymc) for on-demand server startup
- `nix` crate for POSIX signals
- Familiarity with socket activation (`systemd.socket(5)`, `sd_listen_fds(3)`)

---

## Installation

1. Clone & build the binary:
   ```sh
   git clone https://github.com/lucca-pellegrini/minecraft-servers-tmux-wrapper.git
   cd minecraft-servers-tmux-wrapper
   cargo build --release
   ```
2. Install the binary:
   ```sh
   install -D target/release/minecraft-servers-tmux-wrapper ~/.local/bin/minecraft-servers-tmux-wrapper
   chmod +x ~/.local/bin/minecraft-servers-tmux-wrapper
   ```
3. Create your server directories and startup scripts:
   ```
   ~/.local/share/srv/
   ├── lobby/
   │   └── run       # +x script to launch your server
   ├── world1/
   │   └── run
   └── world2/
       └── run
   ```
4. Copy the example systemd units into `~/.config/systemd/user/`:

   - **fabric-servers-wrapper.socket**
     ```ini
     [Unit]
     Description=Socket-activated Minecraft proxy listener

     [Socket]
     ListenStream=25565
     NoDelay=true
     Service=fabric-servers-wrapper.service

     [Install]
     WantedBy=sockets.target
     ```

   - **fabric-servers-wrapper.service**
     ```ini
     [Unit]
     Description=Launch Minecraft servers in tmux & forward traffic
     After=network.target

     [Service]
     ExecStart=%h/.local/bin/fabric-servers-tmux-wrapper
     StandardInput=socket
     StandardOutput=journal
     StandardError=journal
     Restart=on-failure

     [Install]
     WantedBy=default.target
     ```

5. Enable and start:
   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now fabric-servers-wrapper.socket
   ```

---

## How It Works

1. **Socket Activation**
   systemd listens on TCP port 25565 and passes the listener on FD 3 to the
   daemon. (See `systemd.socket(5)`, `sd_listen_fds(3)`, and Lennart
   Poettering’s blog: https://0pointer.de/blog/projects/socket-activation.html)

2. **Startup & tmux Session**
   - Reads `$XDG_RUNTIME_DIR` (defaults to `/tmp`) and creates a tmux socket
     `fabric-servers.sock`.
   - Kills any existing `fabric-servers` session on that socket.
   - Scans `~/.local/share/srv/` for subdirectories; each becomes a tmux
     window, launched by its `./run` script.
   - If a directory matches `.*Velocity.*`, it’s started first (so the proxy is
     started before — or at least logically prior to — the game servers).
     Therefore, it's window ID will always be 0 (or 1, depending on your tmux
     configuration).

3. **Proxy Health-check**
   - Waits up to 25 seconds for the Velocity proxy to accept connections on
     `127.0.0.1:25564`.
   - If the proxy fails to start, the daemon exits with error.

4. **Traffic Forwarding**
   - Accepts incoming Minecraft client connections on FD 3 (from systemd).
   - For each client, opens a TCP tunnel to `127.0.0.1:25564` and proxies all
     bytes bidirectionally.

5. **Cleanup on Proxy Exit**
   - Continuously polls the proxy every 5 seconds.
   - On proxy failure, spawns shutdown tasks for each tmux‐managed server:
     - Grace period: 10 seconds
     - Retries SIGTERM, SIGINT, SIGHUP (3× each, 10 seconds in between)
     - Finally SIGKILL if still alive

---

## Configuration

Currently, key parameters are hardcoded in `src/main.rs` as constants:

- `SRV_DIR`: `".local/share/srv"`
- `SRV_STARTUP_SCRIPT`: `"./run"`
- `TMUX_SESSION`: `"fabric-servers"`
- `PROXY_PORT`: `25564`
- `BUFFER_TIMEOUT`: `25` (seconds)

To customize paths, session names or timeouts, fork/patch and recompile.

---

## Usage

- Add new servers:
  1. Create directory under `~/.local/share/srv/`
  2. Place a `run` script (executable) that starts your
     Vanilla/Fabric/Paper/Spigot/whatever server
- The next socket activation (first client connect) will spin up all servers.

Monitoring & logs:

- `journalctl --user -u fabric-servers-wrapper.service -f`
- tmux session: `tmux -S $XDG_RUNTIME_DIR/fabric-servers.sock ls`
- Attach to a server pane:
  ```sh
  tmux -S $XDG_RUNTIME_DIR/fabric-servers.sock attach -t fabric-servers:<window-name>
  ```

---

## Copyright

This project is licensed under the Apache License, Version 2.0. See the
[LICENSE](LICENSE) file for more details.

---

## See also

- [systemd.unit(5)](https://man.archlinux.org/man/systemd.unit.5)
- [systemd.service(5)](https://man.archlinux.org/man/systemd.service.5)
- [systemd.socket(5)](https://man.archlinux.org/man/systemd.socket.5)
- [sd_listen_fds(3)](https://man.archlinux.org/man/sd_listen_fds.3.en)
- (Lennart Poettering’s blog post)[https://0pointer.de/blog/projects/socket-activation.html]
- [lazymc](https://github.com/timvisee/lazymc)
- [Velocity proxy](https://papermc.io/software/velocity/)
- [ViaVersion](https://viaversion.com/) for protocol compatibility
