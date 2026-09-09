use horus_dev_relay::Relay;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bonjour type phones browse for (`NSBonjourServices` / Android NSD).
const REGISTRY_MDNS_TYPE: &str = "_horus-reg._tcp.local.";

fn main() -> std::io::Result<()> {
    // 0.0.0.0 so a physical phone on Wi‑Fi can reach this Mac; use 127.0.0.1 for simulator-only.
    let addr = std::env::var("HORUS_RELAY_ADDR").unwrap_or_else(|_| "0.0.0.0:8787".into());
    let ttl_secs: u64 = std::env::var("HORUS_RELAY_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        // Default 7 days — matches horus_config message_ttl_days for async offline delivery.
        .unwrap_or(7 * 24 * 60 * 60);
    let persist = std::env::var("HORUS_RELAY_STATE").unwrap_or_else(|_| "relay_state.json".into());

    let relay = Arc::new(Mutex::new(Relay::with_persist(
        Duration::from_secs(ttl_secs),
        persist,
    )));
    let listener = TcpListener::bind(&addr)?;
    let port = listener.local_addr()?.port();

    println!("horus-relay listening on http://{addr}");
    // Keep daemon alive for the process lifetime (drop would unregister).
    let _mdns = advertise_mdns(port);
    if addr.starts_with("0.0.0.0") || addr.starts_with("[::]") {
        if let Some(ip) = primary_ipv4() {
            println!("LAN URL (auto-discovered by the app): http://{ip}:{port}");
        }
        println!("Phones browse Bonjour {REGISTRY_MDNS_TYPE} — no manual Registry URL.");
    }
    for stream in listener.incoming().flatten() {
        let relay = Arc::clone(&relay);
        std::thread::spawn(move || {
            let _ = handle(stream, relay);
        });
    }
    Ok(())
}

fn primary_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) if !v4.is_loopback() => Some(v4.to_string()),
        _ => None,
    }
}

fn advertise_mdns(port: u16) -> Option<ServiceDaemon> {
    let ip = primary_ipv4()?;
    let mdns = ServiceDaemon::new().ok()?;
    let host = format!("{ip}.local.");
    let service = ServiceInfo::new(
        REGISTRY_MDNS_TYPE,
        "horus-dev",
        &host,
        ip.as_str(),
        port,
        None::<std::collections::HashMap<String, String>>,
    )
    .ok()?;
    mdns.register(service).ok()?;
    println!("mDNS advertised {REGISTRY_MDNS_TYPE} → http://{ip}:{port}");
    Some(mdns)
}

fn read_http(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find_headers_end(&buf) {
            let content_len = content_length(&buf[..i]).unwrap_or(0);
            let body_start = i + 4;
            while buf.len() < body_start + content_len {
                let n = stream.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            break;
        }
        if buf.len() > 1024 * 1024 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(headers: &[u8]) -> Option<usize> {
    let s = String::from_utf8_lossy(headers);
    for line in s.lines() {
        if let Some(v) = line
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
        {
            return v.trim().parse().ok();
        }
    }
    None
}

fn handle(mut stream: TcpStream, relay: Arc<Mutex<Relay>>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let req = read_http(&mut stream)?;
    let Some((head, body)) = req.split_once("\r\n\r\n") else {
        return respond(&mut stream, 400, b"bad request");
    };

    let mut lines = head.lines();
    let Some(first) = lines.next() else {
        return respond(&mut stream, 400, b"bad request");
    };
    let parts: Vec<_> = first.split_whitespace().collect();
    if parts.len() < 2 {
        return respond(&mut stream, 400, b"bad request");
    }

    match (parts[0], parts[1]) {
        ("GET", "/health") => respond(&mut stream, 200, b"ok"),
        ("POST", "/forward") => {
            let body = body.as_bytes();
            match relay.lock().unwrap().forward_onion(body) {
                Ok(_) => respond(&mut stream, 202, b"forwarded"),
                Err(_) => respond(&mut stream, 400, b"bad onion"),
            }
        }
        ("POST", path) if path.starts_with("/queues/") => {
            let queue = &path["/queues/".len()..];
            let body = body.as_bytes().to_vec();
            match relay.lock().unwrap().send(queue, body) {
                Ok(()) => respond(&mut stream, 202, b"queued"),
                Err(_) => respond(&mut stream, 400, b"bad queue"),
            }
        }
        ("GET", path) if path.starts_with("/queues/") => {
            let queue = &path["/queues/".len()..];
            match relay.lock().unwrap().poll(queue) {
                Some((lease, body)) => respond_leased(&mut stream, &lease, &body),
                None => respond(&mut stream, 204, b""),
            }
        }
        ("POST", path) if path.starts_with("/ack/") => {
            let id = &path["/ack/".len()..];
            if relay.lock().unwrap().ack(id) {
                respond(&mut stream, 200, b"acked")
            } else {
                respond(&mut stream, 404, b"unknown lease")
            }
        }
        // Waku bridge API (same queues; content-topic path from clients).
        ("POST", path) if path.starts_with("/waku/v1/queues/") => {
            let queue = &path["/waku/v1/queues/".len()..];
            let body = body.as_bytes().to_vec();
            match relay.lock().unwrap().send(queue, body) {
                Ok(()) => respond(&mut stream, 202, b"queued"),
                Err(_) => respond(&mut stream, 400, b"bad queue"),
            }
        }
        ("GET", path) if path.starts_with("/waku/v1/queues/") => {
            let queue = &path["/waku/v1/queues/".len()..];
            match relay.lock().unwrap().poll(queue) {
                Some((lease, body)) => respond_leased(&mut stream, &lease, &body),
                None => respond(&mut stream, 204, b""),
            }
        }
        ("POST", path) if path.starts_with("/waku/v1/ack/") => {
            let id = &path["/waku/v1/ack/".len()..];
            if relay.lock().unwrap().ack(id) {
                respond(&mut stream, 200, b"acked")
            } else {
                respond(&mut stream, 404, b"unknown lease")
            }
        }
        ("PUT", path) if path.starts_with("/registry/") => {
            let name = &path["/registry/".len()..];
            // JSON: {"salt_b64":"...","findable":true,"contact":"horus://invite/..."}
            #[derive(serde::Deserialize)]
            struct ClaimBody {
                salt_b64: String,
                #[serde(default)]
                findable: bool,
                contact: Option<String>,
            }
            let parsed: Result<ClaimBody, _> = serde_json::from_str(body.trim());
            match parsed {
                Ok(req) => {
                    let salt = match base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        req.salt_b64.trim(),
                    ) {
                        Ok(s) => s,
                        Err(_) => {
                            respond(&mut stream, 400, b"bad salt")?;
                            return Ok(());
                        }
                    };
                    match relay.lock().unwrap().claim_username(
                        name,
                        &salt,
                        req.findable,
                        req.contact.as_deref(),
                    ) {
                        Ok(()) => respond(&mut stream, 201, b"claimed"),
                        Err(horus_dev_relay::ClaimError::Registry(
                            horus_registry::RegistryError::Taken,
                        )) => respond(&mut stream, 409, b"taken"),
                        Err(horus_dev_relay::ClaimError::RateLimited) => {
                            respond(&mut stream, 429, b"rate limited")
                        }
                        Err(_) => respond(&mut stream, 400, b"bad claim"),
                    }
                }
                Err(_) => respond(&mut stream, 400, b"bad json"),
            }
        }
        ("GET", path) if path.starts_with("/registry/") && path.ends_with("/taken") => {
            let name = &path["/registry/".len()..path.len() - "/taken".len()];
            let taken = relay.lock().unwrap().username_taken(name);
            respond(&mut stream, 200, if taken { b"1" } else { b"0" })
        }
        ("GET", path) if path.starts_with("/registry/") => {
            let name = &path["/registry/".len()..];
            match relay.lock().unwrap().resolve_username(name) {
                Some(contact) => respond(&mut stream, 200, contact.as_bytes()),
                None => respond(&mut stream, 404, b"not found"),
            }
        }
        _ => respond(&mut stream, 404, b"not found"),
    }
}

fn respond(stream: &mut TcpStream, code: u16, body: &[u8]) -> std::io::Result<()> {
    respond_headers(stream, code, body, None)
}

fn respond_leased(stream: &mut TcpStream, lease_id: &str, body: &[u8]) -> std::io::Result<()> {
    respond_headers(stream, 200, body, Some(lease_id))
}

fn respond_headers(
    stream: &mut TcpStream,
    code: u16,
    body: &[u8],
    lease_id: Option<&str>,
) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        _ => "OK",
    };
    let lease_hdr = lease_id
        .map(|id| format!("X-Horus-Lease: {id}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\n{lease_hdr}Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}
