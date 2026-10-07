//! Minimal HTTP/1.1 GET client for loopback web servers (in-app previews of an agent's dev
//! server). Only `http://localhost`, `127.0.0.1` and `[::1]`; redirects are returned, not
//! followed.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use yonder_proto::app::ApiError;

/// Whole-request deadline.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// Largest response (headers plus body, as received) accepted.
pub const MAX_RESPONSE: usize = 20 * 1024 * 1024;
/// Response headers passed on to the client.
const KEEP_HEADERS: &[&str] = &["content-type", "content-length", "location", "cache-control", "last-modified", "etag"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Host to connect to (brackets stripped for IPv6).
    pub host: String,
    pub port: u16,
    /// `Host` header value.
    pub host_header: String,
    /// Path and query, starting with `/`.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Parses `url` and checks it points at this host's loopback interface.
pub fn parse_target(url: &str) -> Result<Target, ApiError> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or_else(|| ApiError::invalid("not an absolute URL"))?;
    if !scheme.eq_ignore_ascii_case("http") {
        return Err(ApiError::forbidden("only http:// URLs can be fetched"));
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    if authority.contains('@') {
        return Err(ApiError::forbidden("URLs with credentials are not allowed"));
    }
    let (host, port) = if let Some(after) = authority.strip_prefix('[') {
        let close = after.find(']').ok_or_else(|| ApiError::invalid("bad IPv6 host"))?;
        let host = &after[..close];
        let port = match &after[close + 1..] {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or_else(|| ApiError::invalid("bad port"))?),
        };
        (format!("[{host}]"), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (authority.to_string(), None),
        }
    };
    let host_lc = host.to_ascii_lowercase();
    if !matches!(host_lc.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        return Err(ApiError::forbidden("only localhost, 127.0.0.1 and [::1] can be fetched"));
    }
    let port: u16 = match port {
        None | Some("") => 80,
        Some(p) => p.parse().ok().filter(|p| *p != 0).ok_or_else(|| ApiError::invalid("bad port"))?,
    };
    // The fragment is never sent.
    let tail = tail.split('#').next().unwrap_or("");
    let path = if tail.starts_with('/') { tail.to_string() } else { format!("/{tail}") };
    if path.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(ApiError::invalid("URL contains spaces or control characters"));
    }
    let host_header = if port == 80 { host_lc.clone() } else { format!("{host_lc}:{port}") };
    let connect = host_lc.trim_start_matches('[').trim_end_matches(']').to_string();
    Ok(Target { host: connect, port, host_header, path })
}

/// GETs `url` (see the module docs for what is allowed).
pub async fn get(url: &str) -> Result<HttpResponse, ApiError> {
    let target = parse_target(url)?;
    match tokio::time::timeout(TIMEOUT, fetch(&target)).await {
        Ok(r) => r,
        Err(_) => Err(ApiError::busy(format!("{url} did not answer within {} s", TIMEOUT.as_secs()))),
    }
}

/// Tailscale's fixed MagicDNS address; only the Tailscale interface routes it.
const TAILSCALE_QUAD100: std::net::Ipv4Addr = std::net::Ipv4Addr::new(100, 100, 100, 100);

/// This host's Tailscale IPv4 address. Other overlays (NetBird, some VPNs) also use
/// 100.64.0.0/10, so the interface is picked by routing: the source address the OS chooses for
/// Tailscale's MagicDNS address (a connected UDP socket sends nothing). Falls back to an
/// interface named like Tailscale, then to the only address in the range.
pub fn tailnet_ipv4() -> Option<std::net::Ipv4Addr> {
    let routed = std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|s| s.connect((TAILSCALE_QUAD100, 53)).and(s.local_addr()))
        .ok()
        .and_then(|a| match a.ip() {
            std::net::IpAddr::V4(v4) if is_tailnet(v4) && v4 != TAILSCALE_QUAD100 => Some(v4),
            _ => None,
        });
    if routed.is_some() {
        return routed;
    }
    let all: Vec<(String, std::net::Ipv4Addr)> = if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) if is_tailnet(v4) => Some((i.name, v4)),
            _ => None,
        })
        .collect();
    let named = all.iter().find(|(name, _)| name.to_ascii_lowercase().contains("tailscale"));
    named.or(if all.len() == 1 { all.first() } else { None }).map(|(_, ip)| *ip)
}

fn is_tailnet(ip: std::net::Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xc0) == 64
}

/// `url` (a loopback URL accepted by [`get`]) with its host replaced by `ip`.
pub fn rewrite_host(url: &str, ip: std::net::Ipv4Addr) -> Result<(String, u16), ApiError> {
    let t = parse_target(url)?;
    let authority = if t.port == 80 { ip.to_string() } else { format!("{ip}:{}", t.port) };
    let fragment = url.find('#').map(|i| &url[i..]).unwrap_or("");
    Ok((format!("http://{authority}{}{fragment}", t.path), t.port))
}

/// The Tailscale form of a loopback `url`, and whether its port answers there.
pub async fn tailnet_url(url: &str) -> Result<(String, bool), ApiError> {
    let ip = tailnet_ipv4().ok_or_else(|| ApiError::not_found("this host has no Tailscale address"))?;
    let (out, port) = rewrite_host(url, ip)?;
    let probe = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect((ip, port))).await;
    Ok((out, matches!(probe, Ok(Ok(_)))))
}

async fn fetch(t: &Target) -> Result<HttpResponse, ApiError> {
    let mut stream = connect(t).await?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept-Encoding: identity\r\nUser-Agent: yonder\r\nAccept: */*\r\n\r\n",
        t.path, t.host_header
    );
    stream.write_all(req.as_bytes()).await.map_err(|e| ApiError::internal(format!("send request: {e}")))?;
    let mut raw = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await.map_err(|e| ApiError::internal(format!("read response: {e}")))?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > MAX_RESPONSE {
            return Err(ApiError::invalid("response too large"));
        }
        // With a known length there is no need to wait for the server to close.
        if let Some(done) = complete_len(&raw)? {
            raw.truncate(done);
            break;
        }
    }
    parse_response(&raw)
}

async fn connect(t: &Target) -> Result<TcpStream, ApiError> {
    // `localhost` may resolve to ::1 and 127.0.0.1; dev servers often listen on only one.
    let addrs: Vec<std::net::SocketAddr> = match t.host.as_str() {
        "localhost" => vec![(std::net::Ipv4Addr::LOCALHOST, t.port).into(), (std::net::Ipv6Addr::LOCALHOST, t.port).into()],
        h => vec![std::net::SocketAddr::new(h.parse().map_err(|_| ApiError::invalid("bad host"))?, t.port)],
    };
    let mut last = None;
    for a in addrs {
        match TcpStream::connect(a).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    let e = last.map(|e| e.to_string()).unwrap_or_default();
    Err(ApiError::not_found(format!("nothing answers on {}: {e}", t.host_header)))
}

/// Position of the end of the header block (after `\r\n\r\n`).
fn header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Total length of a complete response, when it is framed by `Content-Length` (or has no body).
fn complete_len(raw: &[u8]) -> Result<Option<usize>, ApiError> {
    let Some(end) = header_end(raw) else { return Ok(None) };
    let (status, headers) = parse_head(&raw[..end])?;
    if no_body(status) {
        return Ok(Some(end));
    }
    if let Some(te) = header(&headers, "transfer-encoding") {
        // Complete once the last chunk (and trailers) arrived; only checked when the data
        // ends like that, so large bodies are not rescanned on every read.
        let done = te.to_ascii_lowercase().contains("chunked") && raw.ends_with(b"\r\n\r\n") && decode_chunked(&raw[end..]).is_ok();
        return Ok(done.then_some(raw.len()));
    }
    match header(&headers, "content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) if n > MAX_RESPONSE => Err(ApiError::invalid("response too large")),
        Some(n) if raw.len() >= end + n => Ok(Some(end + n)),
        _ => Ok(None),
    }
}

fn no_body(status: u16) -> bool {
    (100..200).contains(&status) || status == 204 || status == 304
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Status line and headers (names lowercased) of a header block.
fn parse_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>), ApiError> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/1.") {
        return Err(ApiError::invalid(format!("not an HTTP/1.x response: {}", status_line.chars().take(40).collect::<String>())));
    }
    let status = parts.next().and_then(|s| s.parse::<u16>().ok()).filter(|s| (100..1000).contains(s)).ok_or_else(|| ApiError::invalid("bad status line"))?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    Ok((status, headers))
}

/// Decodes a `Transfer-Encoding: chunked` body (trailers are dropped).
pub fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, ApiError> {
    let bad = || ApiError::invalid("malformed chunked body");
    let mut out = Vec::new();
    loop {
        let eol = data.windows(2).position(|w| w == b"\r\n").ok_or_else(bad)?;
        let line = std::str::from_utf8(&data[..eol]).map_err(|_| bad())?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| bad())?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 || &data[size..size + 2] != b"\r\n" {
            return Err(bad());
        }
        if out.len() + size > MAX_RESPONSE {
            return Err(ApiError::invalid("response too large"));
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

/// Parses a complete raw response.
pub fn parse_response(raw: &[u8]) -> Result<HttpResponse, ApiError> {
    let end = header_end(raw).ok_or_else(|| ApiError::invalid("incomplete HTTP response"))?;
    let (status, headers) = parse_head(&raw[..end])?;
    let rest = &raw[end..];
    let chunked = header(&headers, "transfer-encoding").map(|v| v.to_ascii_lowercase().contains("chunked")).unwrap_or(false);
    let body = if no_body(status) {
        Vec::new()
    } else if chunked {
        decode_chunked(rest)?
    } else if let Some(v) = header(&headers, "content-length") {
        let n: usize = v.trim().parse().map_err(|_| ApiError::invalid("bad content-length"))?;
        if rest.len() < n {
            return Err(ApiError::invalid("response ended early"));
        }
        rest[..n].to_vec()
    } else {
        // Delimited by the connection closing.
        rest.to_vec()
    };
    let mut kept: Vec<(String, String)> = headers.into_iter().filter(|(k, _)| KEEP_HEADERS.contains(&k.as_str())).collect();
    // The body is passed on decoded: describe it, not the wire format.
    kept.retain(|(k, _)| k != "content-length");
    if !no_body(status) {
        kept.push(("content-length".into(), body.len().to_string()));
    }
    Ok(HttpResponse { status, headers: kept, body })
}

#[cfg(test)]
mod tests {
    #[test]
    fn rewrites_loopback_to_tailnet() {
        let ip = std::net::Ipv4Addr::new(100, 85, 1, 2);
        assert_eq!(rewrite_host("http://localhost:5173/app/?x=1#top", ip).unwrap(), ("http://100.85.1.2:5173/app/?x=1#top".to_string(), 5173));
        assert_eq!(rewrite_host("http://127.0.0.1/", ip).unwrap().0, "http://100.85.1.2/");
        assert_eq!(rewrite_host("http://[::1]:8080", ip).unwrap().0, "http://100.85.1.2:8080/");
        assert!(rewrite_host("http://example.com/", ip).is_err());
        assert!(is_tailnet(ip) && is_tailnet(std::net::Ipv4Addr::new(100, 127, 0, 1)));
        assert!(!is_tailnet(std::net::Ipv4Addr::new(100, 128, 0, 1)) && !is_tailnet(std::net::Ipv4Addr::new(192, 168, 1, 2)));
    }

    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn host_allow_list() {
        let t = parse_target("http://localhost:5173/app/?x=1#top").unwrap();
        assert_eq!(t, Target { host: "localhost".into(), port: 5173, host_header: "localhost:5173".into(), path: "/app/?x=1".into() });
        let t = parse_target("http://127.0.0.1").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.path.as_str(), t.host_header.as_str()), ("127.0.0.1", 80, "/", "127.0.0.1"));
        let t = parse_target("http://[::1]:8080?q").unwrap();
        assert_eq!((t.host.as_str(), t.port, t.path.as_str(), t.host_header.as_str()), ("::1", 8080, "/?q", "[::1]:8080"));
        assert_eq!(parse_target("HTTP://LOCALHOST:3000/").unwrap().host, "localhost");

        for bad in [
            "https://localhost/",
            "file:///etc/passwd",
            "http://example.com/",
            "http://127.0.0.2/",
            "http://0.0.0.0:80/",
            "http://[::2]/",
            "http://localhost.evil.com/",
            "http://evil.com@localhost/",
            "http://localhost@evil.com/",
            "http://192.168.1.1/",
        ] {
            let e = parse_target(bad).unwrap_err();
            assert_eq!(e.code, "forbidden", "{bad}: {e}");
        }
        for bad in ["localhost:3000", "http://localhost:99999/", "http://localhost:x/", "http://localhost/a b"] {
            assert_eq!(parse_target(bad).unwrap_err().code, "invalid", "{bad}");
        }
    }

    #[test]
    fn chunked_decoding() {
        let body = decode_chunked(b"4\r\nWiki\r\n6;ext=1\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\nX-Trailer: 1\r\n\r\n").unwrap();
        assert_eq!(body, b"Wikipedia in \r\n\r\nchunks.");
        assert_eq!(decode_chunked(b"0\r\n\r\n").unwrap(), b"");
        assert!(decode_chunked(b"5\r\nabc\r\n0\r\n\r\n").is_err());
        assert!(decode_chunked(b"zz\r\nabc\r\n").is_err());
        assert!(decode_chunked(b"3\r\nabc\r\n").is_err(), "missing last chunk");
    }

    #[test]
    fn header_parsing() {
        let raw = b"HTTP/1.1 302 Found\r\nLocation: /login\r\nSet-Cookie: a=b\r\nCache-Control: no-store\r\nETag: \"x\"\r\nContent-Length: 3\r\n\r\nabcEXTRA";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 302);
        assert_eq!(r.body, b"abc");
        assert_eq!(
            r.headers,
            vec![
                ("location".to_string(), "/login".to_string()),
                ("cache-control".to_string(), "no-store".to_string()),
                ("etag".to_string(), "\"x\"".to_string()),
                ("content-length".to_string(), "3".to_string()),
            ]
        );
        let r = parse_response(b"HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\n\r\nuntil close").unwrap();
        assert_eq!(r.body, b"until close");
        assert_eq!(r.headers[0], ("content-type".to_string(), "text/plain".to_string()));
        let r = parse_response(b"HTTP/1.1 304 Not Modified\r\nETag: y\r\n\r\n").unwrap();
        assert!(r.body.is_empty());
        assert!(parse_response(b"SSH-2.0-OpenSSH\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());
        assert_eq!(complete_len(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi").unwrap(), Some(40));
        assert_eq!(complete_len(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nh").unwrap(), None);
        assert_eq!(complete_len(b"HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n").unwrap_err().message, "response too large");
    }

    /// Serves `response` once on a loopback port and returns the port and the request seen.
    async fn serve_once(response: Vec<u8>, close: bool) -> (u16, tokio::sync::oneshot::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 1024];
            while header_end(&req).is_none() {
                let n = s.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            s.write_all(&response).await.unwrap();
            let _ = tx.send(String::from_utf8(req).unwrap());
            if !close {
                // Keep the connection open: the client must stop on Content-Length.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        (port, rx)
    }

    #[tokio::test]
    async fn fetch_content_length() {
        let (port, _req) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\nX-Other: 1\r\n\r\nhello".to_vec(), false).await;
        let started = std::time::Instant::now();
        let r = get(&format!("http://127.0.0.1:{port}/index.html?v=2")).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(4), "did not wait for the server to close");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hello");
        assert_eq!(r.headers, vec![("content-type".into(), "text/html".into()), ("content-length".into(), "5".into())]);
    }

    #[tokio::test]
    async fn fetch_chunked_and_request_shape() {
        // Left open by the server: the client stops after the last chunk.
        let (port, req) =
            serve_once(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n3\r\n{\"a\r\n4\r\n\":1}\r\n0\r\n\r\n".to_vec(), false).await;
        let started = std::time::Instant::now();
        let r = get(&format!("http://localhost:{port}/api")).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(r.body, b"{\"a\":1}");
        assert!(r.headers.contains(&("content-length".into(), "7".into())));
        let req = req.await.unwrap();
        assert!(req.starts_with("GET /api HTTP/1.1\r\n"), "{req}");
        assert!(req.contains(&format!("\r\nHost: localhost:{port}\r\n")), "{req}");
        assert!(req.contains("\r\nConnection: close\r\n"));
        assert!(req.contains("\r\nAccept-Encoding: identity\r\n"));
        assert!(req.contains("\r\nUser-Agent: yonder\r\n"));
    }

    #[tokio::test]
    async fn fetch_redirect_not_followed_and_refused_port() {
        let (port, req) = serve_once(b"HTTP/1.1 301 Moved\r\nLocation: http://example.com/\r\nContent-Length: 0\r\n\r\n".to_vec(), true).await;
        let r = get(&format!("http://127.0.0.1:{port}/")).await.unwrap();
        assert_eq!(r.status, 301);
        assert!(r.headers.contains(&("location".into(), "http://example.com/".into())));
        req.await.unwrap();
        // Nothing listens on a port that was just closed.
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let e = get(&format!("http://127.0.0.1:{port}/")).await.unwrap_err();
        assert_eq!(e.code, "not_found");
    }
}
