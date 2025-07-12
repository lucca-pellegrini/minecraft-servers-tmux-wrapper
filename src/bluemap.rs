// SPDX-License-Identifier: Apache-2.0

use crate::config::{BLUEMAP_MOD_PORT, BLUEMAP_WEBROOT, SRV_DIR};
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::TcpStream,
    time::timeout,
};

// Handle one incoming Bluemap/TCP connection.
// Try to proxy to a live Bluemap backend on localhost:BLUEMAP_MOD_PORT (2s timeout), if this
// fails, respond over HTTP as a minimal static‐file server with 204 for missing tiles or live
// data, 404 elsewhere.
pub async fn handle_connection(mut client: TcpStream) -> anyhow::Result<()> {
    // Attempt to establish a live proxy connection to the Bluemap backend.
    match timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", BLUEMAP_MOD_PORT)),
    )
    .await
    {
        Ok(Ok(mut backend)) => {
            let _ = copy_bidirectional(&mut client, &mut backend).await;
            return Ok(());
        }
        _ => {
            serve_static(client).await?;
            return Ok(());
        }
    }
}

async fn serve_static(mut client: TcpStream) -> io::Result<()> {
    // Read request into a small buffer
    let mut buf = [0u8; 8192];
    let n = client.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let mut lines = req.lines();
    let first = lines.next().unwrap_or("");

    // Expect the request line to be in the format: "GET /path HTTP/1.1".
    let parts: Vec<_> = first.split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(());
    }
    let mut path = parts[1].to_string();
    if let Some(pos) = path.find('?') {
        path.truncate(pos);
    }
    if path.ends_with('/') {
        path.push_str("index.html");
    }

    // Securely join the requested path under the BLUEMAP_WEBROOT directory.
    let full_path = sanitize_path(&BLUEMAP_WEBROOT.lock().unwrap(), &path);

    // Determine the type of request based on the path.
    let is_tiles = path.starts_with("/maps/") && path.contains("/tiles/");
    let is_live = path.starts_with("/maps/") && path.contains("/live/");

    // Locate requested file.
    let (file_path, encoding_gzip) = if file_exists(&full_path).await {
        (full_path.clone(), false)
    } else {
        // Attempt to locate the file with a .gz extension if the normal file is not found.
        let gz = full_path.with_extension(
            full_path
                .extension()
                .map(|e| format!("{}.gz", e.to_string_lossy()))
                .unwrap_or_else(|| "gz".into()),
        );
        if file_exists(&gz).await {
            (gz, true)
        } else {
            (full_path.clone(), false)
        }
    };

    // Determine the HTTP status code and response body based on file existence.
    let (status, body) = if file_exists(&file_path).await {
        // 200 + body
        let data = fs::read(&file_path).await?;
        (200, Some(data))
    } else if is_tiles || is_live {
        // 204 – no content but “success”
        (204, None)
    } else {
        (404, None)
    };

    // Write the HTTP response to the client.
    // Write HTTP code.
    let status_line = match status {
        200 => "HTTP/1.1 200 OK\r\n",
        204 => "HTTP/1.1 204 No Content\r\n",
        404 => "HTTP/1.1 404 Not Found\r\n",
        _ => "HTTP/1.1 500 Internal\r\n",
    };
    client.write_all(status_line.as_bytes()).await?;

    // Write the HTTP headers.
    if status == 200 {
        let mime = mime_from_path(&file_path);
        client
            .write_all(format!("Content-Type: {}\r\n", mime).as_bytes())
            .await?;
        if encoding_gzip {
            client.write_all(b"Content-Encoding: gzip\r\n").await?;
        }
        client
            .write_all(format!("Content-Length: {}\r\n", body.as_ref().unwrap().len()).as_bytes())
            .await?;
    }
    client.write_all(b"Connection: close\r\n\r\n").await?;

    // Write the response body to the client, if applicable.
    if let Some(data) = body {
        client.write_all(&data).await?;
    }
    Ok(())
}

fn sanitize_path(root: &str, request_path: &str) -> PathBuf {
    // Sanitize the request path by removing any ".." components to prevent directory traversal.
    let mut out = PathBuf::from(root);
    for comp in Path::new(request_path).components() {
        match comp {
            std::path::Component::Normal(p) => out.push(p),
            _ => {}
        }
    }
    out
}

async fn file_exists(path: &Path) -> bool {
    match fs::metadata(path).await {
        Ok(m) if m.is_file() => true,
        _ => false,
    }
}

fn mime_from_path(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "gz" => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

pub fn find_bluemap_dir() -> Option<String> {
    let path = Path::new(SRV_DIR);
    // Ensure the provided path is a directory.
    if !path.is_dir() {
        return None;
    }

    // Read the entries in the specified directory.
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return None,
    };

    for entry in entries {
        if let Ok(entry) = entry {
            let path = entry.path();
            if path.is_dir() {
                // Check if the directory name does not start with a dot.
                if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                    if !name.starts_with('.') {
                        // Check for the presence of a "bluemap/web" directory within this directory.
                        let bluemap_path = path.join("bluemap/web");
                        if bluemap_path.is_dir() {
                            return Some(bluemap_path.as_os_str().to_str().unwrap().to_string());
                        }
                    }
                }
            }
        }
    }

    None
}
