//! #72: `brainprint web` -- the local Web UI. A loopback-only, GET-only
//! HTTP bridge: it serves the built SvelteKit page (one self-contained
//! `index.html`, embedded) and a few JSON endpoints that pass the daemon's
//! answers through with their `brainprint_core::present` message keys.
//!
//! Nothing here decides a fact: every value is a daemon response, the
//! words come from the shared catalogue, the evidence body is the CLI's
//! compact rendering. No endpoint writes anything -- no mutation, no
//! source, no shell, no recovery -- and the browser never names a daemon
//! endpoint or a filesystem path. Stopping this server never stops the
//! daemon: it only ever opens client connections to it.

mod api;

#[cfg(test)]
mod tests;

use std::{collections::HashMap, net::SocketAddr, time::Duration};

use brainprint_core::{
    present::Locale,
    protocol::{EndpointPaths, endpoint::global_config_path},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use crate::surface::Daemon;

/// The built page (`web/` → `npm run build`), checked in so no Node is
/// needed to build or run Brainprint.
const INDEX_HTML: &str = include_str!("../../../../web/build/index.html");

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(workspace: &str, port: u16, locale: Option<String>) -> i32 {
    let endpoint = match EndpointPaths::resolve() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("brainprint: {error}");
            return 5;
        }
    };
    // #89: start the daemon if it is not running; the screen itself
    // reports a daemon that is down or incompatible.
    if let Err(error @ crate::client::CliError::AutoStart(_)) =
        crate::client::ensure_daemon(&endpoint).await
    {
        eprintln!("brainprint: {error}");
    }
    let config = global_config_path();
    let locale = locale
        .or_else(|| config.as_deref().and_then(crate::tui::saved_locale))
        .map_or(Locale::En, |tag| Locale::from_tag(&tag));
    let daemon = Daemon {
        endpoint,
        workspace: crate::query::absolute_workspace(workspace),
        config,
    };
    let listener = match listen(port).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("brainprint: cannot listen on 127.0.0.1:{port}: {error}");
            return 1;
        }
    };
    let address = listener.local_addr().expect("bound address");
    println!("Brainprint Web UI: http://{address}/  (Ctrl+C stops it; brainprintd keeps running)");
    tokio::select! {
        () = serve(listener, daemon, locale) => 0,
        _ = tokio::signal::ctrl_c() => 0,
    }
}

/// Loopback only: never a wildcard address.
pub async fn listen(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
}

/// Serve until the listener fails. Each connection gets its own task: a
/// browser opens idle preconnects, and one of them must never hold up the
/// page's real requests.
pub async fn serve(listener: TcpListener, daemon: Daemon, locale: Locale) {
    let Ok(address) = listener.local_addr() else {
        return;
    };
    let daemon = std::sync::Arc::new(daemon);
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let daemon = std::sync::Arc::clone(&daemon);
        tokio::spawn(async move { handle(stream, address, &daemon, locale).await });
    }
}

struct Reply {
    status: &'static str,
    content_type: &'static str,
    body: String,
}

impl Reply {
    fn json(body: &serde_json::Value) -> Self {
        Self {
            status: "200 OK",
            content_type: "application/json; charset=utf-8",
            body: body.to_string(),
        }
    }

    fn error(status: &'static str, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::json!({ "error": message }).to_string(),
        }
    }
}

async fn handle(mut stream: TcpStream, address: SocketAddr, daemon: &Daemon, locale: Locale) {
    let reply = match tokio::time::timeout(READ_TIMEOUT, read_head(&mut stream)).await {
        Ok(Some(head)) => route(&head, address, daemon, locale).await,
        Ok(None) => Reply::error("400 Bad Request", "malformed request"),
        Err(_) => return,
    };
    let head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; \
         style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(reply.body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A request head, bounded; a body is never read (only GET is served).
struct Head {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
}

async fn read_head(stream: &mut TcpStream) -> Option<Head> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        if buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let text = std::str::from_utf8(&buffer).ok()?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next()?.split(' ');
    let method = request.next()?.to_owned();
    let target = request.next()?;
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = HashMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':')?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    Some(Head {
        method,
        path: path.to_owned(),
        query: parse_query(raw_query),
        headers,
    })
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((decode(key)?, decode(value)?))
        })
        .collect()
}

/// `application/x-www-form-urlencoded` decoding; `None` on a bad escape.
fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = text.get(index + 1..index + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                index += 2;
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8(out).ok()
}

/// Only this server's own origin, by address or `localhost` -- a page
/// from anywhere else (or a rebound DNS name) gets nothing.
fn same_origin(head: &Head, address: SocketAddr) -> bool {
    let port = address.port();
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let host_ok = head
        .headers
        .get("host")
        .is_some_and(|host| allowed.iter().any(|allowed| allowed == host));
    let origin_ok = head.headers.get("origin").is_none_or(|origin| {
        allowed
            .iter()
            .any(|allowed| origin == &format!("http://{allowed}"))
    });
    host_ok && origin_ok
}

async fn route(head: &Head, address: SocketAddr, daemon: &Daemon, locale: Locale) -> Reply {
    if !same_origin(head, address) {
        return Reply::error("403 Forbidden", "only this local UI's own origin is served");
    }
    if head.method != "GET" {
        return Reply::error("405 Method Not Allowed", "this UI is read-only: GET only");
    }
    let param = |name: &str| head.query.get(name).map(String::as_str);
    match head.path.as_str() {
        "/" | "/index.html" => Reply {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            body: INDEX_HTML.to_owned(),
        },
        "/api/catalogue" => Reply::json(&api::catalogue(
            param("locale").map_or(locale, Locale::from_tag),
        )),
        "/api/status" => Reply::json(&api::status(daemon).await),
        "/api/work" => Reply::json(&api::work(daemon).await),
        "/api/rules" => Reply::json(&api::rules(daemon).await),
        "/api/find" => match param("q").map(str::trim).filter(|q| !q.is_empty()) {
            Some(text) => Reply::json(&api::find(daemon, text).await),
            None => Reply::error("400 Bad Request", "q is required"),
        },
        path @ ("/api/inspect" | "/api/relations" | "/api/impact") => {
            let Some(target) = param("target").and_then(api::target) else {
                return Reply::error("400 Bad Request", "target must be a candidate's token");
            };
            let continuation = match param("continuation").map(api::continuation) {
                None => None,
                Some(Some(continuation)) => Some(continuation),
                Some(None) => {
                    return Reply::error("400 Bad Request", "continuation is not a valid token");
                }
            };
            Reply::json(&match path {
                "/api/inspect" => api::inspect(daemon, target, continuation).await,
                "/api/relations" => api::relations(daemon, target, continuation).await,
                _ => {
                    let change = param("change").and_then(|c| c.parse().ok()).unwrap_or(0);
                    api::impact(daemon, target, change, continuation).await
                }
            })
        }
        _ => Reply::error("404 Not Found", "no such page"),
    }
}
