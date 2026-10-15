//! A small, synchronous HTTP/1.1 server on `std::net` and `httparse`, enough
//! for the index API and the demo A2A agent. Every connection gets a thread
//! and carries one request, then closes. Streams (server-sent events for A2A)
//! go out with chunked transfer encoding, one chunk per event, so each event
//! reaches the client at once.
//!
//! Slow or greedy clients cannot hold the server: at most
//! [`Limits::connections`] connections are served at once and
//! [`Limits::per_ip`] from one address (IPv6 by /64), others get 503 at once.
//! The request head must arrive within [`Limits::head`] and the whole request
//! within [`Limits::request`], however slowly the bytes trickle in, and a
//! response of bytes must be taken within [`Limits::response`].

use std::collections::HashMap;
use std::io::{self, BufWriter, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::{debug, warn};

/// The longest request head (request line and headers).
const MAX_HEAD: usize = 16 * 1024;
/// The largest request body, enough for a manifest, a profile or one photo.
pub const MAX_BODY: usize = 3 * 1024 * 1024;
/// The longest a single read may block, within the deadlines.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// How much one server takes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Connections served at once.
    pub connections: usize,
    /// Connections served at once from one address.
    pub per_ip: usize,
    /// Time for the request line and the headers.
    pub head: Duration,
    /// Time for the whole request, body included.
    pub request: Duration,
    /// Time for writing a response that is not a stream.
    pub response: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connections: 128,
            per_ip: 64,
            head: Duration::from_secs(10),
            request: Duration::from_secs(60),
            response: Duration::from_secs(60),
        }
    }
}

/// The key one client counts under: its IPv4 address, or the /64 of its IPv6 address.
pub fn client_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

/// Open connections, in total and per client.
#[derive(Default)]
struct Slots {
    open: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

impl Slots {
    /// Takes a slot for `ip`, None when the server or the client is full.
    fn take(self: &Arc<Self>, ip: Option<IpAddr>, limits: &Limits) -> Option<Slot> {
        let key = ip.map(client_key);
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let per_ip = key.and_then(|k| open.1.get(&k).copied()).unwrap_or(0);
        if open.0 >= limits.connections || per_ip >= limits.per_ip {
            return None;
        }
        open.0 += 1;
        if let Some(k) = key {
            *open.1.entry(k).or_default() += 1;
        }
        Some(Slot { slots: self.clone(), key })
    }
}

/// One open connection, given back when it drops.
struct Slot {
    slots: Arc<Slots>,
    key: Option<IpAddr>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut open = self.slots.open.lock().unwrap_or_else(|e| e.into_inner());
        open.0 = open.0.saturating_sub(1);
        if let Some(k) = self.key
            && let Some(n) = open.1.get_mut(&k)
        {
            *n -= 1;
            if *n == 0 {
                open.1.remove(&k);
            }
        }
    }
}

/// One parsed request.
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub method: String,
    /// The path without the query, percent-decoded.
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub peer: Option<SocketAddr>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn query(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The non-empty path segments, so `/v1/manifests/a/b` gives
    /// `["v1", "manifests", "a", "b"]`.
    pub fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }
}

/// Writes the events of a streamed response.
pub struct EventSink<'a> {
    out: &'a mut dyn Write,
}

impl EventSink<'_> {
    /// Sends one server-sent event with `data` as its only data line.
    pub fn data(&mut self, data: &str) -> io::Result<()> {
        let event = format!("data: {}\n\n", data.replace('\n', "\ndata: "));
        write!(self.out, "{:x}\r\n{event}\r\n", event.len())?;
        self.out.flush()
    }
}

type Stream = Box<dyn FnOnce(&mut EventSink) -> io::Result<()> + Send>;

pub enum Body {
    Bytes(Vec<u8>),
    /// Server-sent events, written by the closure while the client waits.
    Events(Stream),
}

pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Body,
}

impl Response {
    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self { status, content_type: content_type.into(), body: Body::Bytes(body) }
    }

    pub fn json<T: Serialize + ?Sized>(status: u16, value: &T) -> Self {
        let body = serde_json::to_vec_pretty(value).unwrap_or_else(|_| b"{}".to_vec());
        Self::bytes(status, "application/json", body)
    }

    /// `{"error": message}` with the status.
    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, &serde_json::json!({ "error": message.into() }))
    }

    pub fn events(stream: impl FnOnce(&mut EventSink) -> io::Result<()> + Send + 'static) -> Self {
        Self { status: 200, content_type: "text/event-stream".into(), body: Body::Events(Box::new(stream)) }
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// A server on a background thread. Dropping it stops accepting.
pub struct Server {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Binds `addr` (port 0 picks a free port) and serves on a background thread.
    pub fn start(addr: impl ToSocketAddrs, handler: Handler) -> io::Result<Self> {
        Self::from_listener(TcpListener::bind(addr)?, handler)
    }

    /// Serves on a listener that is already bound, for handlers that need to
    /// know their own address.
    pub fn from_listener(listener: TcpListener, handler: Handler) -> io::Result<Self> {
        Self::with_limits(listener, handler, Limits::default())
    }

    /// Serves on a bound listener with other limits than the default.
    pub fn with_limits(listener: TcpListener, handler: Handler, limits: Limits) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread =
            thread::Builder::new().name(format!("http-{}", addr.port())).spawn(move || accept_loop(listener, handler, flag, limits))?;
        Ok(Self { addr, stop, thread: Some(thread) })
    }

    /// `http://<addr>`.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Serves until the process ends.
    pub fn wait(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept call, which then sees the flag.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn accept_loop(listener: TcpListener, handler: Handler, stop: Arc<AtomicBool>, limits: Limits) {
    let slots = Arc::new(Slots::default());
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                warn!("accept failed ({e})");
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        let Some(slot) = slots.take(stream.peer_addr().ok().map(|a| a.ip()), &limits) else {
            // A short answer fits the socket buffer, so this never blocks the loop for long.
            let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
            let _ = stream.write_all(BUSY);
            let _ = stream.shutdown(Shutdown::Both);
            continue;
        };
        let handler = handler.clone();
        if let Err(e) = thread::Builder::new().name("http-conn".into()).spawn(move || {
            let _slot = slot;
            connection(stream, &handler, &limits)
        }) {
            warn!("cannot start a connection thread ({e})");
        }
    }
}

/// The answer when the server or the client has too many connections open.
const BUSY: &[u8] = concat!(
    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 49\r\n",
    "Retry-After: 5\r\nConnection: close\r\n\r\n{\"error\": \"too many connections, try again soon\"}"
)
.as_bytes();

fn connection(mut stream: TcpStream, handler: &Handler, limits: &Limits) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let peer = stream.peer_addr().ok();
    let response = match read_request(&mut stream, limits) {
        Ok(Some(mut request)) => {
            request.peer = peer;
            debug!("{} {}", request.method, request.path);
            handler(request)
        }
        Ok(None) => return,
        Err((status, message)) => Response::error(status, message),
    };
    if let Err(e) = write_response(&mut stream, response, Instant::now() + limits.response) {
        debug!("writing a response failed ({e})");
    }
    let _ = stream.shutdown(Shutdown::Both);
}

/// Reads into `chunk` before `deadline`, waiting at most [`READ_TIMEOUT`] at a time.
fn read_until(stream: &mut TcpStream, chunk: &mut [u8], deadline: Instant, what: &str) -> Result<usize, (u16, String)> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err((408, format!("the {what} took too long")));
    }
    let _ = stream.set_read_timeout(Some(left.min(READ_TIMEOUT)));
    stream.read(chunk).map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => (408, format!("the {what} took too long")),
        _ => (408, format!("cannot read the {what} ({e})")),
    })
}

/// Reads one request. `Ok(None)` when the client closed without sending one.
fn read_request(stream: &mut TcpStream, limits: &Limits) -> Result<Option<Request>, (u16, String)> {
    let started = Instant::now();
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = read_until(stream, &mut chunk, started + limits.head, "request head")?;
        if n == 0 {
            return if buf.is_empty() { Ok(None) } else { Err((400, "the request ended early".into())) };
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            break end + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err((431, "the request head is too large".into()));
        }
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed.parse(&buf[..head_end]) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err((400, "incomplete request head".into())),
        Err(e) => return Err((400, format!("malformed request ({e})"))),
    }
    let method = parsed.method.unwrap_or("GET").to_string();
    let target = parsed.path.unwrap_or("/");
    let headers: Vec<(String, String)> =
        parsed.headers.iter().map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).trim().to_string())).collect();
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    if header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        return Err((411, "send the body with a Content-Length".into()));
    }
    let length: usize = match header("content-length") {
        Some(v) => v.parse().map_err(|_| (400, format!("bad Content-Length {v:?}")))?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err((413, format!("the body is larger than {MAX_BODY} bytes")));
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let n = read_until(stream, &mut chunk, started + limits.request, "request body")?;
        if n == 0 {
            return Err((400, "the body ended early".into()));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    Ok(Some(Request { method, path: percent_decode(path), query, headers, body, peer: None }))
}

/// Writes a response. Bytes must be taken before `deadline`, streams run as
/// long as the handler writes.
fn write_response(stream: &mut TcpStream, response: Response, deadline: Instant) -> io::Result<()> {
    let reason = reason(response.status);
    match response.body {
        Body::Bytes(body) => {
            let head = format!(
                "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
                response.status,
                response.content_type,
                body.len()
            );
            for part in [head.as_bytes(), &body] {
                for piece in part.chunks(64 * 1024) {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "the client took too long to read the response"));
                    }
                    stream.set_write_timeout(Some(left.min(WRITE_TIMEOUT)))?;
                    stream.write_all(piece)?;
                }
            }
            stream.flush()
        }
        Body::Events(events) => {
            let mut out = BufWriter::new(stream);
            write!(
                out,
                "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nCache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                response.status, response.content_type
            )?;
            out.flush()?;
            let result = events(&mut EventSink { out: &mut out });
            // The last chunk ends the body, also after an error in the stream.
            out.write_all(b"0\r\n\r\n")?;
            out.flush()?;
            result
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Other",
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Decodes `%XX` escapes and `+` as a space.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                Ok(b) => {
                    out.push(b);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Encodes everything except unreserved characters, for query values.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_encodes() {
        assert_eq!(percent_decode("eu+data%20residency"), "eu data residency");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
        assert_eq!(percent_decode(&percent_encode("Grüße & mehr")), "Grüße & mehr");
    }

    #[test]
    fn serves_bytes_and_events() {
        let handler: Handler = Arc::new(|req: Request| match req.segments().as_slice() {
            ["echo"] => Response::bytes(200, "text/plain", req.body.clone()),
            ["q"] => Response::bytes(200, "text/plain", req.query("x").unwrap_or("").as_bytes().to_vec()),
            ["events"] => Response::events(|sink| {
                sink.data("one")?;
                sink.data("{\"two\":2}")
            }),
            _ => Response::error(404, "nothing here"),
        });
        let server = Server::start("127.0.0.1:0", handler).unwrap();
        let config = ureq::Agent::config_builder().http_status_as_error(false).build();
        let http = ureq::Agent::new_with_config(config);
        let mut r = http.post(&format!("{}/echo", server.url())).send("hello").unwrap();
        assert_eq!(r.body_mut().read_to_string().unwrap(), "hello");
        let mut r = http.get(&format!("{}/q?x=a%20b", server.url())).call().unwrap();
        assert_eq!(r.body_mut().read_to_string().unwrap(), "a b");
        let mut r = http.get(&format!("{}/events", server.url())).call().unwrap();
        assert_eq!(r.headers().get("content-type").unwrap(), "text/event-stream");
        assert_eq!(r.body_mut().read_to_string().unwrap(), "data: one\n\ndata: {\"two\":2}\n\n");
        let r = http.get(&format!("{}/missing", server.url())).call().unwrap();
        assert_eq!(r.status().as_u16(), 404);
        assert_eq!(r.headers().get("x-content-type-options").unwrap(), "nosniff", "browsers must not guess served files");
    }

    fn echo() -> Handler {
        Arc::new(|req: Request| Response::bytes(200, "text/plain", req.body.clone()))
    }

    /// The whole answer of the server on a raw connection.
    fn answer(stream: &mut TcpStream) -> String {
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut text = String::new();
        let _ = stream.read_to_string(&mut text);
        text
    }

    #[test]
    fn a_trickling_client_hits_the_deadline() {
        let limits = Limits { head: Duration::from_millis(400), request: Duration::from_millis(800), ..Limits::default() };
        let server = Server::with_limits(TcpListener::bind("127.0.0.1:0").unwrap(), echo(), limits).unwrap();
        // One byte every 100 ms keeps every single read short, the deadline still ends it.
        let mut stream = TcpStream::connect(server.addr).unwrap();
        let started = Instant::now();
        for b in b"GET / HTTP/1.1\r\nHost: x\r\n" {
            if stream.write_all(&[*b]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            if started.elapsed() > Duration::from_secs(2) {
                break;
            }
        }
        let text = answer(&mut stream);
        assert!(text.starts_with("HTTP/1.1 408"), "{text}");
        assert!(started.elapsed() < Duration::from_secs(4));

        // The same for a body that never completes.
        let mut stream = TcpStream::connect(server.addr).unwrap();
        stream.write_all(b"POST / HTTP/1.1\r\nContent-Length: 100\r\n\r\nabc").unwrap();
        let started = Instant::now();
        let text = answer(&mut stream);
        assert!(text.starts_with("HTTP/1.1 408") && text.contains("request body"), "{text}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn connections_beyond_the_limit_are_refused_at_once() {
        let limits = Limits { connections: 2, per_ip: 2, head: Duration::from_secs(5), ..Limits::default() };
        let server = Server::with_limits(TcpListener::bind("127.0.0.1:0").unwrap(), echo(), limits).unwrap();
        // Two idle connections hold both slots.
        let idle: Vec<TcpStream> = (0..2).map(|_| TcpStream::connect(server.addr).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(200));
        let mut third = TcpStream::connect(server.addr).unwrap();
        let text = answer(&mut third);
        assert!(text.starts_with("HTTP/1.1 503") && text.ends_with("try again soon\"}"), "{text}");
        // Closing them frees the slots.
        drop(idle);
        std::thread::sleep(Duration::from_millis(300));
        let mut fourth = TcpStream::connect(server.addr).unwrap();
        fourth.write_all(b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nok").unwrap();
        assert!(answer(&mut fourth).starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn clients_count_by_address_and_ipv6_by_prefix() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        assert_eq!(client_key(a), client_key(b));
        assert_eq!(client_key("::ffff:10.0.0.1".parse().unwrap()), "10.0.0.1".parse::<IpAddr>().unwrap());
        let slots = Arc::new(Slots::default());
        let limits = Limits { connections: 10, per_ip: 1, ..Limits::default() };
        let first = slots.take(Some(a), &limits);
        assert!(first.is_some() && slots.take(Some(b), &limits).is_none(), "one /64 is one client");
        assert!(slots.take(Some("10.0.0.1".parse().unwrap()), &limits).is_some());
        drop(first);
        assert!(slots.take(Some(b), &limits).is_some());
    }
}
