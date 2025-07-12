// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashMap, env, io, os::fd::FromRawFd};

use log::trace;
use tokio::net::TcpListener;

pub async fn get_systemd_sockets() -> anyhow::Result<HashMap<u16, TcpListener>, io::Error> {
    let listen_fds = env::var("LISTEN_FDS")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    trace!("Listening on {} file descriptors", listen_fds);

    let mut result = HashMap::new();

    for fd in 3..3 + listen_fds {
        let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };

        if let Ok(local_addr) = std_listener.local_addr() {
            std_listener.set_nonblocking(true)?;
            trace!(
                "Storing listener {} from descriptor {} for port {}",
                std_listener.local_addr().unwrap(),
                fd,
                local_addr.port()
            );

            let tokio_listener = TcpListener::from_std(std_listener)?;
            result.insert(local_addr.port(), tokio_listener);
        }
    }

    Ok(result)
}
