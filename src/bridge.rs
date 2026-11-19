//! Authenticated loopback bridge between the POM proxy and the OpenMausBot
//! server.
//!
//! The node mounts this bridge at `/api/ui/plugins/openmausbot/proxy` and adds
//! `x-pom-plugin-token`; the bridge refuses anything without it, then talks to
//! OpenMausBot on loopback as a local caller (OpenMausBot's own trust model:
//! loopback is the owner). OpenMausBot's web app is built for the root of its
//! own origin, so the bridge mounts it under the prefix: it rewrites the root
//! paths in the HTML, scripts and styles it serves, injects a small script
//! that prefixes the paths the app builds at run time (fetch, XHR,
//! EventSource, WebSocket, history), lets the page be framed by its own
//! origin and passes WebSockets and streams through untouched.
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

const DEFAULT_PREFIX: &str = "/api/ui/plugins/openmausbot/proxy";
const MAX_HEAD: usize = 64 * 1024;
const MAX_REWRITE: usize = 64 * 1024 * 1024;

/// Request headers never sent to OpenMausBot: credentials from the browser,
/// hop-by-hop headers and what would make it see a remote caller.
const DROPPED_REQUEST: &[&str] = &[
    "host",
    "x-pom-plugin-token",
    "x-forwarded-prefix",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-real-ip",
    "forwarded",
    "cookie",
    "authorization",
    "api-key",
    "origin",
    "referer",
    "accept-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "upgrade",
    "te",
    "sec-fetch-site",
    "sec-fetch-mode",
    "sec-fetch-dest",
    "sec-fetch-user",
];

pub struct Bridge {
    pub port: u16,
    pub token: String,
    stop: Arc<AtomicBool>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag and releases the port.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Starts the bridge on `127.0.0.1:0` in front of OpenMausBot on `upstream`.
pub fn start(upstream: u16) -> io::Result<Bridge> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = random_token()?;
    let stop = Arc::new(AtomicBool::new(false));
    let (accept_token, accept_stop) = (token.clone(), Arc::clone(&stop));
    thread::spawn(move || {
        for stream in listener.incoming() {
            if accept_stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let token = accept_token.clone();
            thread::spawn(move || {
                let _ = handle(stream, upstream, &token);
            });
        }
    });
    Ok(Bridge { port, token, stop })
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| io::Error::other("no system randomness"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct Head {
    first: String,
    headers: Vec<(String, String)>,
}

impl Head {
    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Reads a message head; the reader keeps whatever follows it.
fn read_head(reader: &mut impl BufRead) -> io::Result<Head> {
    let mut first = String::new();
    let mut total = reader.read_line(&mut first)?;
    if total == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        total += read;
        if read == 0 || total > MAX_HEAD {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    Ok(Head {
        first: first.trim_end_matches(['\r', '\n']).to_owned(),
        headers,
    })
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// A request another site made the browser send. POM's session cookie is
/// `SameSite=Strict`, so the node already refuses those; this is the second
/// wall, because the bridge presents every request it passes as OpenMausBot's
/// local owner.
fn cross_site(head: &Head) -> bool {
    head.get("sec-fetch-site")
        .is_some_and(|site| !matches!(site.to_ascii_lowercase().as_str(), "same-origin" | "none"))
}

/// The mount point the node announces, kept only when it is a plain path.
pub fn clean_prefix(value: Option<&str>) -> String {
    let prefix = value.unwrap_or(DEFAULT_PREFIX).trim_end_matches('/');
    let plain = prefix.starts_with('/')
        && !prefix.starts_with("//")
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte));
    if plain && !prefix.is_empty() {
        prefix.to_owned()
    } else {
        DEFAULT_PREFIX.to_owned()
    }
}

/// The path OpenMausBot sees: the request target without the mount point.
fn upstream_target(target: &str, prefix: &str) -> String {
    let rest = target.strip_prefix(prefix).unwrap_or(target);
    if rest.is_empty() || rest.starts_with('?') {
        format!("/{rest}")
    } else if rest.starts_with('/') {
        rest.to_owned()
    } else {
        format!("/{rest}")
    }
}

fn handle(client: TcpStream, upstream: u16, token: &str) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let head = read_head(&mut reader)?;
    let mut writer = client.try_clone()?;
    let authorized = head
        .get("x-pom-plugin-token")
        .is_some_and(|given| constant_time_eq(given.as_bytes(), token.as_bytes()));
    if !authorized {
        writer.write_all(
            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    if cross_site(&head) {
        writer.write_all(
            b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )?;
        return Ok(());
    }
    let mut parts = head.first.splitn(3, ' ');
    let (method, target) = (parts.next().unwrap_or("GET"), parts.next().unwrap_or("/"));
    let prefix = clean_prefix(head.get("x-forwarded-prefix"));
    let path = upstream_target(target, &prefix);
    let websocket = head
        .get("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let local = format!("127.0.0.1:{upstream}");

    let mut request = format!("{method} {path} HTTP/1.1\r\nhost: {local}\r\n");
    for (name, value) in &head.headers {
        if !DROPPED_REQUEST
            .iter()
            .any(|dropped| name.eq_ignore_ascii_case(dropped))
        {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if websocket {
        request.push_str("connection: Upgrade\r\nupgrade: websocket\r\n\r\n");
    } else {
        request.push_str("accept-encoding: identity\r\nconnection: close\r\n\r\n");
    }

    let mut server = TcpStream::connect(&local)?;
    server.write_all(request.as_bytes())?;
    client.set_read_timeout(None)?;

    if websocket {
        return pipe_both(reader, client, server);
    }

    // The request body, if any, goes on while the reply is read.
    let length = head
        .get("content-length")
        .and_then(|value| value.parse::<u64>().ok());
    let chunked = head
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    if length.is_some_and(|length| length > 0) || chunked {
        let mut body_target = server.try_clone()?;
        thread::spawn(move || {
            let _ = match length {
                Some(length) => io::copy(&mut reader.take(length), &mut body_target),
                None => io::copy(&mut reader, &mut body_target),
            };
        });
    }

    let mut from_server = BufReader::new(server.try_clone()?);
    let reply = read_head(&mut from_server)?;
    let status = reply.first.split(' ').nth(1).unwrap_or("");
    let content_type = reply.get("content-type").unwrap_or("").to_ascii_lowercase();
    let kind = rewrite_kind(&content_type);
    let mut out = format!("{}\r\n", reply.first);
    for (name, value) in &reply.headers {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "x-frame-options" | "connection" | "keep-alive" => {}
            "content-length" | "transfer-encoding" if kind.is_some() && status == "200" => {}
            "content-security-policy" => {
                out.push_str(&format!("{name}: {}\r\n", frame_by_self(value)));
            }
            "location" if value.starts_with('/') && !value.starts_with("//") => {
                out.push_str(&format!("{name}: {prefix}{value}\r\n"));
            }
            _ => out.push_str(&format!("{name}: {value}\r\n")),
        }
    }
    out.push_str("connection: close\r\n");

    match kind.filter(|_| status == "200") {
        Some(kind) => {
            let body = read_body(&mut from_server, &reply)?;
            let body = rewrite(kind, &body, &prefix);
            out.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
            writer.write_all(out.as_bytes())?;
            writer.write_all(&body)?;
        }
        None => {
            out.push_str("\r\n");
            writer.write_all(out.as_bytes())?;
            io::copy(&mut from_server, &mut writer)?;
        }
    }
    writer.flush()?;
    let _ = server.shutdown(Shutdown::Both);
    let _ = client.shutdown(Shutdown::Both);
    Ok(())
}

fn pipe_both(
    mut from_client: BufReader<TcpStream>,
    client: TcpStream,
    server: TcpStream,
) -> io::Result<()> {
    let mut to_server = server.try_clone()?;
    let (client_end, server_end) = (client.try_clone()?, server.try_clone()?);
    let upward = thread::spawn(move || {
        let _ = io::copy(&mut from_client, &mut to_server);
        let _ = server_end.shutdown(Shutdown::Both);
    });
    let (mut from_server, mut to_client) = (server, client);
    let _ = io::copy(&mut from_server, &mut to_client);
    let _ = client_end.shutdown(Shutdown::Both);
    let _ = upward.join();
    Ok(())
}

fn read_body(reader: &mut impl BufRead, head: &Head) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let chunked = head
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if size == 0 {
                break;
            }
            if body.len() + size > MAX_REWRITE {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..])?;
            let mut end = [0u8; 2];
            reader.read_exact(&mut end)?;
        }
    } else if let Some(length) = head
        .get("content-length")
        .and_then(|value| value.parse::<u64>().ok())
    {
        reader
            .take(length.min(MAX_REWRITE as u64))
            .read_to_end(&mut body)?;
    } else {
        reader.take(MAX_REWRITE as u64).read_to_end(&mut body)?;
    }
    Ok(body)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Html,
    Script,
    Style,
    Manifest,
}

fn rewrite_kind(content_type: &str) -> Option<Kind> {
    if content_type.starts_with("text/html") {
        Some(Kind::Html)
    } else if content_type.contains("javascript") {
        Some(Kind::Script)
    } else if content_type.starts_with("text/css") {
        Some(Kind::Style)
    } else if content_type.contains("manifest+json") {
        Some(Kind::Manifest)
    } else {
        None
    }
}

/// OpenMausBot forbids framing (`frame-ancestors 'none'`); POM shows it in a
/// frame of its own origin, so only that origin may.
fn frame_by_self(policy: &str) -> String {
    let mut directives: Vec<String> = policy
        .split(';')
        .map(str::trim)
        .filter(|directive| {
            !directive.is_empty()
                && !directive
                    .to_ascii_lowercase()
                    .starts_with("frame-ancestors")
        })
        .map(str::to_owned)
        .collect();
    directives.push("frame-ancestors 'self'".into());
    directives.join("; ")
}

/// Replaces `pattern` followed by `/` + a non-slash character with the same
/// text carrying the prefix: root paths gain the mount point, protocol-
/// relative URLs (`//host`) and already prefixed paths are kept.
fn prefix_after(text: &str, pattern: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(pattern) {
        let after = &rest[at + pattern.len()..];
        out.push_str(&rest[..at + pattern.len()]);
        let root = after.starts_with('/') && !after.starts_with("//");
        let done = after.starts_with(prefix) && after[prefix.len()..].starts_with('/');
        if root && !done {
            out.push_str(prefix);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

fn rewrite(kind: Kind, body: &[u8], prefix: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(body) else {
        return body.to_vec();
    };
    let text = match kind {
        Kind::Html => {
            let mut text = text.to_owned();
            for pattern in [" src=\"", " href=\"", " content=\""] {
                text = prefix_after(&text, pattern, prefix);
            }
            inject_shim(&text, prefix)
        }
        Kind::Script => {
            let mut text = text.to_owned();
            for pattern in ["src:\"", "href:\"", "return\""] {
                text = prefix_after(&text, pattern, prefix);
            }
            text.replace("\"/assets/", &format!("\"{prefix}/assets/"))
                .replace("`/assets/", &format!("`{prefix}/assets/"))
        }
        Kind::Style => prefix_after(text, "url(", prefix),
        Kind::Manifest => {
            let mut text = text.to_owned();
            for pattern in [
                "\"src\": \"",
                "\"start_url\": \"",
                "\"scope\": \"",
                "\"id\": \"",
            ] {
                text = prefix_after(&text, pattern, prefix);
            }
            text
        }
    };
    text.into_bytes()
}

/// Runs before the app's modules: every root path the app hands the browser
/// at run time gains the mount point.
const SHIM: &str = r#"(()=>{const P=__PREFIX__;const own=p=>p===P||p.startsWith(P+"/");
const fix=u=>{if(u==null)return u;const s=String(u);if(s.startsWith("/")&&!s.startsWith("//"))return own(s)?s:P+s;
try{const x=new URL(s,location.href);const same=x.host===location.host&&/^(https?|wss?):$/.test(x.protocol);
if(same&&!own(x.pathname)){x.pathname=P+x.pathname;return typeof u==="string"?x.href:x}}catch(e){}return u};
const F=window.fetch;window.fetch=function(i,o){if(i instanceof Request){const u=fix(i.url);if(u!==i.url)i=new Request(u,i)}else i=fix(i);return F.call(this,i,o)};
const O=XMLHttpRequest.prototype.open;XMLHttpRequest.prototype.open=function(m,u,...r){return O.call(this,m,fix(u),...r)};
const wrap=C=>{if(!C)return C;const W=function(u,...r){return new C(fix(u),...r)};W.prototype=C.prototype;Object.setPrototypeOf(W,C);return W};
window.EventSource=wrap(window.EventSource);window.WebSocket=wrap(window.WebSocket);
for(const k of["pushState","replaceState"]){const h=history[k];history[k]=function(s,t,u){return h.call(this,s,t,u==null?u:fix(u))}}
if(navigator.sendBeacon){const B=navigator.sendBeacon.bind(navigator);navigator.sendBeacon=(u,d)=>B(fix(u),d)}
if(!own(location.pathname))history.replaceState(history.state,"",P+"/");})();"#;

fn inject_shim(html: &str, prefix: &str) -> String {
    let script = format!(
        "<script>{}</script>",
        SHIM.replace(
            "__PREFIX__",
            &serde_json::to_string(prefix).unwrap_or_default()
        )
    );
    match html.find("<head>") {
        Some(at) => format!("{}{script}{}", &html[..at + 6], &html[at + 6..]),
        None => format!("{script}{html}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const P: &str = "/api/ui/plugins/openmausbot/proxy";

    #[test]
    fn prefix_is_a_plain_path_or_the_default() {
        assert_eq!(clean_prefix(Some("/api/ui/plugins/openmausbot/proxy/")), P);
        assert_eq!(clean_prefix(Some("/x\"</script>")), DEFAULT_PREFIX);
        assert_eq!(clean_prefix(Some("//evil")), DEFAULT_PREFIX);
        assert_eq!(clean_prefix(None), DEFAULT_PREFIX);
    }

    #[test]
    fn upstream_target_drops_the_mount_point() {
        assert_eq!(upstream_target(P, P), "/");
        assert_eq!(upstream_target(&format!("{P}/"), P), "/");
        assert_eq!(upstream_target(&format!("{P}?a=1"), P), "/?a=1");
        assert_eq!(
            upstream_target(&format!("{P}/api/health"), P),
            "/api/health"
        );
        assert_eq!(upstream_target("/api/health", P), "/api/health");
    }

    #[test]
    fn html_scripts_and_styles_gain_the_prefix_once() {
        let html = r#"<html><head><script type="module" src="/assets/index.js"></script><link href="//cdn/x"></head></html>"#;
        let out = String::from_utf8(rewrite(Kind::Html, html.as_bytes(), P)).unwrap();
        assert!(out.contains(&format!(r#"src="{P}/assets/index.js""#)));
        assert!(out.contains(r#"href="//cdn/x""#));
        assert!(out.starts_with("<html><head><script>"));
        let again = String::from_utf8(rewrite(Kind::Html, out.as_bytes(), P)).unwrap();
        assert_eq!(again.matches(&format!("{P}/assets/index.js")).count(), 1);

        let js = r#"const a="/assets/logo.png";x={src:"/app-icon.svg"};function p(t){return"/"+t};new URL(`/assets/w.js`)"#;
        let out = String::from_utf8(rewrite(Kind::Script, js.as_bytes(), P)).unwrap();
        assert!(out.contains(&format!(r#""{P}/assets/logo.png""#)));
        assert!(out.contains(&format!(r#"src:"{P}/app-icon.svg""#)));
        assert!(out.contains(&format!(r#"return"{P}/"+t"#)));
        assert!(out.contains(&format!("`{P}/assets/w.js`")));

        let css = "a{background:url(/assets/f.woff2)}b{background:url(data:x)}";
        let out = String::from_utf8(rewrite(Kind::Style, css.as_bytes(), P)).unwrap();
        assert_eq!(
            out,
            format!("a{{background:url({P}/assets/f.woff2)}}b{{background:url(data:x)}}")
        );
    }

    #[test]
    fn manifest_paths_gain_the_prefix() {
        let manifest =
            r#"{"id": "/", "start_url": "/", "icons": [{ "src": "/app-icon-192.png" }]}"#;
        let out = String::from_utf8(rewrite(Kind::Manifest, manifest.as_bytes(), P)).unwrap();
        assert!(out.contains(&format!(r#""src": "{P}/app-icon-192.png""#)));
        assert!(out.contains(&format!(r#""start_url": "{P}/""#)));
    }

    #[test]
    fn framing_is_allowed_to_the_own_origin_only() {
        assert_eq!(
            frame_by_self("default-src 'self'; frame-ancestors 'none'"),
            "default-src 'self'; frame-ancestors 'self'"
        );
        assert_eq!(
            frame_by_self("frame-ancestors 'none'"),
            "frame-ancestors 'self'"
        );
    }

    fn upstream_echo() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let stream = stream.unwrap();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let head = read_head(&mut reader).unwrap();
                    let mut stream = stream;
                    if head.first.contains("/page") {
                        let body = "<html><head></head><img src=\"/a.png\"></html>";
                        write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nx-frame-options: DENY\r\ncontent-security-policy: frame-ancestors 'none'\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n", body.len()).unwrap();
                        return;
                    }
                    let seen = format!(
                        "{}|host={}|cookie={}|token={}|origin={}|fwd={}",
                        head.first,
                        head.get("host").unwrap_or("-"),
                        head.get("cookie").unwrap_or("-"),
                        head.get("x-pom-plugin-token").unwrap_or("-"),
                        head.get("origin").unwrap_or("-"),
                        head.get("x-forwarded-for").unwrap_or("-"),
                    );
                    let mut body = vec![
                        0u8;
                        head.get("content-length")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0)
                    ];
                    reader.read_exact(&mut body).unwrap();
                    let body = format!("{seen}|body={}", String::from_utf8_lossy(&body));
                    write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}", body.len()).unwrap();
                });
            }
        });
        port
    }

    fn call(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    #[test]
    fn bridge_refuses_without_token_and_forwards_a_local_request() {
        let upstream = upstream_echo();
        let bridge = start(upstream).unwrap();
        let refused = call(
            bridge.port,
            &format!("GET {P}/api/x HTTP/1.1\r\nhost: a\r\n\r\n"),
        );
        assert!(refused.starts_with("HTTP/1.1 401"));

        let reply = call(
            bridge.port,
            &format!(
                "POST {P}/api/x?y=1 HTTP/1.1\r\nhost: pom.example\r\nx-pom-plugin-token: {}\r\nx-forwarded-prefix: {P}\r\nx-forwarded-for: 10.0.0.9\r\ncookie: s=1\r\norigin: https://pom.example\r\ncontent-length: 5\r\n\r\nhello",
                bridge.token
            ),
        );
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.contains("POST /api/x?y=1 HTTP/1.1"), "{reply}");
        assert!(reply.contains(&format!("host=127.0.0.1:{upstream}")));
        assert!(reply.contains("origin=-"));
        assert!(reply.contains("cookie=-|token=-"));
        assert!(reply.contains("fwd=-"));
        assert!(reply.contains("body=hello"));

        let cross = call(
            bridge.port,
            &format!("GET {P}/api/x HTTP/1.1\r\nhost: a\r\nx-pom-plugin-token: {}\r\nsec-fetch-site: cross-site\r\n\r\n", bridge.token),
        );
        assert!(cross.starts_with("HTTP/1.1 403"), "{cross}");

        let page = call(
            bridge.port,
            &format!(
                "GET {P}/page HTTP/1.1\r\nhost: a\r\nx-pom-plugin-token: {}\r\n\r\n",
                bridge.token
            ),
        );
        let lower = page.to_ascii_lowercase();
        assert!(!lower.contains("x-frame-options"));
        assert!(lower.contains("frame-ancestors 'self'"));
        assert!(!lower.contains("transfer-encoding"));
        assert!(page.contains(&format!("src=\"{P}/a.png\"")));
    }
}

#[cfg(test)]
mod manual {
    /// Serves a bridge in front of `OMB_PORT` and prints its port and token,
    /// for checking the mounted web app by hand.
    #[test]
    #[ignore]
    fn serve_in_front_of_a_running_server() {
        let upstream = std::env::var("OMB_PORT").unwrap().parse().unwrap();
        let bridge = super::start(upstream).unwrap();
        println!("BRIDGE {} {}", bridge.port, bridge.token);
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
