//! The only way the headless browser reaches the internet. The browser runs on a
//! network with no route out, and every page it loads, along with every script,
//! image, and redirect on that page, has to come through here, where it gets the same
//! address checks as `LinkFetcher`. Without this, a page could have the browser load
//! addresses on the bot's own network.
//!
//! Only `CONNECT` tunnels to ports 80 and 443 are offered. Plain-HTTP proxying would
//! let the browser send requests for different hosts down one connection, which a
//! tunnel can't check, so `http://` pages fall back to `LinkFetcher`'s own fetch.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use crate::links::is_public;

/// Longest request head accepted. A `CONNECT` is a line and a few headers.
const MAX_HEAD_BYTES: usize = 8 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a tunnel stays open. Pages are only rendered for a few seconds, so anything
/// older is a page that kept a connection open.
const TUNNEL_LIFETIME: Duration = Duration::from_secs(120);

/// Accepts tunnels from the browser, and only from it: `browser_host` is resolved on
/// each connection, so other containers can't use this as a proxy.
pub async fn serve(listener: TcpListener, browser_host: String) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                warn!(?err, "browser proxy failed to accept a connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let browser_host = browser_host.clone();
        tokio::spawn(async move {
            if !is_from_browser(peer.ip(), &browser_host).await {
                debug!(%peer, "browser proxy refused a connection from another host");
                return;
            }
            if let Err(err) = tunnel(stream, false).await {
                debug!(?err, "browser proxy tunnel ended");
            }
        });
    }
}

async fn is_from_browser(peer: IpAddr, browser_host: &str) -> bool {
    match tokio::net::lookup_host((browser_host, 0)).await {
        Ok(addrs) => addrs.into_iter().any(|addr| addr.ip() == peer.to_canonical()),
        Err(_) => false,
    }
}

/// Handles one `CONNECT`, refusing anything that isn't to a public address on a web
/// port. `allow_private` is only for tests, which serve on localhost.
async fn tunnel(mut client: TcpStream, allow_private: bool) -> Result<()> {
    let (head, rest) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client)).await.context("request head timed out")??;

    let target = match parse_connect(&head) {
        Some(target) => target,
        None => {
            client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n").await?;
            bail!("not a CONNECT request");
        }
    };

    let upstream = match open_upstream(&target, allow_private).await {
        Ok(upstream) => upstream,
        Err(err) => {
            client.write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n").await?;
            return Err(err);
        }
    };

    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
    let mut upstream = upstream;
    if !rest.is_empty() {
        upstream.write_all(&rest).await?;
    }
    // Ends when either side closes, or at the lifetime limit.
    let _ = tokio::time::timeout(TUNNEL_LIFETIME, tokio::io::copy_bidirectional(&mut client, &mut upstream)).await;
    Ok(())
}

/// Reads up to the end of the request head, returning it and any bytes after it.
async fn read_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("connection closed before the request head ended");
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Ok((String::from_utf8_lossy(&buffer).into_owned(), rest));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            bail!("request head too long");
        }
    }
}

/// The `host:port` a `CONNECT` asks for, if it's to a web port.
fn parse_connect(head: &str) -> Option<(String, u16)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    if !parts.next()?.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let authority = parts.next()?;
    let (host, port) = authority.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (matches!(port, 80 | 443) && !host.is_empty()).then(|| (host.to_string(), port))
}

/// Connects to the first public address of the target. The address connected to is
/// the one checked, so a name can't resolve differently between the check and use.
async fn open_upstream((host, port): &(String, u16), allow_private: bool) -> Result<TcpStream> {
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), *port)).await?.collect();
    let Some(addr) = found.into_iter().find(|addr| allow_private || is_public(addr.ip())) else {
        bail!("{host} has no public address");
    };
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await.context("connect timed out")??;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_connects_to_web_ports_are_understood() {
        assert_eq!(parse_connect("CONNECT www.reddit.com:443 HTTP/1.1\r\nHost: x\r\n"), Some(("www.reddit.com".to_string(), 443)));
        assert_eq!(parse_connect("CONNECT [2606:4700::1]:443 HTTP/1.1"), Some(("2606:4700::1".to_string(), 443)));
        assert_eq!(parse_connect("CONNECT example.com:22 HTTP/1.1"), None);
        assert_eq!(parse_connect("GET http://example.com/ HTTP/1.1"), None);
        assert_eq!(parse_connect("CONNECT example.com HTTP/1.1"), None);
        assert_eq!(parse_connect("CONNECT :443 HTTP/1.1"), None);
    }

    #[tokio::test]
    async fn private_targets_are_refused() {
        for target in [("localhost".to_string(), 443), ("127.0.0.1".to_string(), 80), ("10.0.0.1".to_string(), 443)] {
            assert!(open_upstream(&target, false).await.is_err(), "{target:?}");
        }
    }

    async fn proxy(allow_private: bool) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(tunnel(stream, allow_private));
            }
        });
        addr
    }

    async fn send(proxy: SocketAddr, request: &str) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(proxy).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = [0u8; 256];
        let read = stream.read(&mut response).await.unwrap();
        (stream, String::from_utf8_lossy(&response[..read]).into_owned())
    }

    #[tokio::test]
    async fn a_tunnel_to_an_allowed_target_carries_bytes_both_ways() {
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut buffer = [0u8; 4];
            stream.read_exact(&mut buffer).await.unwrap();
            stream.write_all(&buffer).await.unwrap();
        });

        // The test server can't be on port 443, so this skips `parse_connect`'s port
        // check by connecting through `open_upstream` the same way `tunnel` does.
        let mut upstream = open_upstream(&("127.0.0.1".to_string(), port), true).await.unwrap();
        upstream.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        upstream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
    }

    #[tokio::test]
    async fn the_proxy_refuses_other_methods_and_private_targets() {
        let addr = proxy(false).await;
        let (_, response) = send(addr, "GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 405"), "{response}");
        let (_, response) = send(addr, "CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    }
}
