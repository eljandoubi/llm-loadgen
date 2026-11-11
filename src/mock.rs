//! A mock OpenAI-compatible streaming server, to test the load generator against and to try it
//! without a GPU.
//!
//! It behaves like an inference server in the ways that matter for latency measurements: a fixed
//! number of generation slots with the rest of the requests waiting in line, a time to first
//! token, a time between tokens, and optional faults (a server-wide stall, an error status, a
//! connection that dies mid-stream).

use crate::json::{self, Value};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A moment, counted from when the server started, during which it does nothing at all: no
/// request is taken up and no stream makes progress. Think of a long GC pause or a frozen VM.
#[derive(Clone, Copy, Debug)]
pub struct Stall {
    pub after: Duration,
    pub length: Duration,
}

#[derive(Clone, Debug)]
pub struct MockConfig {
    /// Wait between getting a slot and the first token.
    pub ttft: Duration,
    /// Wait between two tokens.
    pub itl: Duration,
    /// Tokens to generate when the request does not say.
    pub default_tokens: u32,
    /// Generations that can run at once; further requests wait for a slot.
    pub slots: usize,
    pub stall: Option<Stall>,
    /// Answer every request with this error status instead of generating.
    pub error_status: Option<u16>,
    /// Close the connection after this many tokens, without finishing the stream.
    pub die_after_tokens: Option<u32>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            ttft: Duration::from_millis(50),
            itl: Duration::from_millis(10),
            default_tokens: 32,
            slots: 8,
            stall: None,
            error_status: None,
            die_after_tokens: None,
        }
    }
}

struct Slots {
    free: Mutex<usize>,
    freed: Condvar,
}

struct SlotGuard<'a>(&'a Slots);

impl Slots {
    fn acquire(&self) -> SlotGuard<'_> {
        let mut free = self.free.lock().unwrap();
        while *free == 0 {
            free = self.freed.wait(free).unwrap();
        }
        *free -= 1;
        SlotGuard(self)
    }
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap() += 1;
        self.0.freed.notify_one();
    }
}

struct Shared {
    cfg: MockConfig,
    slots: Slots,
    started: Instant,
    stop: AtomicBool,
    served: AtomicU64,
}

impl Shared {
    fn wait_out_stall(&self) {
        let Some(stall) = self.cfg.stall else { return };
        let end = stall.after + stall.length;
        loop {
            let now = self.started.elapsed();
            if now < stall.after || now >= end {
                return;
            }
            thread::sleep(end.saturating_sub(now));
        }
    }
}

pub struct Server {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    /// Listen on `addr` (use port 0 to let the system pick one).
    pub fn start(addr: &str, cfg: MockConfig) -> io::Result<Server> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let shared = Arc::new(Shared {
            slots: Slots {
                free: Mutex::new(cfg.slots.max(1)),
                freed: Condvar::new(),
            },
            cfg,
            started: Instant::now(),
            stop: AtomicBool::new(false),
            served: AtomicU64::new(0),
        });
        let accept = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || accept_loop(&listener, &shared))
        };
        Ok(Server {
            addr: local,
            shared,
            accept: Some(accept),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("http://{}/v1/chat/completions", self.addr)
    }

    /// Requests that were answered, whatever the answer.
    pub fn served(&self) -> u64 {
        self.shared.served.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let shared = Arc::clone(shared);
                thread::spawn(move || {
                    let _ = handle(stream, &shared);
                    shared.served.fetch_add(1, Ordering::Relaxed);
                });
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break,
        }
    }
}

struct Request {
    method: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    const MAX: usize = 1 << 20;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        if buf.len() > MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let method = head.split_whitespace().next().unwrap_or("").to_string();
    let length = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
        .min(MAX);
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Ok(Request { method, body })
}

fn respond(stream: &mut TcpStream, status: u16, reason: &str, body: &str) -> io::Result<()> {
    let text = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(text.as_bytes())
}

fn error_body(message: &str) -> String {
    Value::obj([("error", Value::obj([("message", message.into())]))]).to_string()
}

fn handle(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let request = read_request(&mut stream)?;
    shared.wait_out_stall();
    if let Some(status) = shared.cfg.error_status {
        return respond(
            &mut stream,
            status,
            "Mock Error",
            &error_body("the mock was told to fail"),
        );
    }
    if request.method != "POST" {
        return respond(
            &mut stream,
            405,
            "Method Not Allowed",
            &error_body("POST only"),
        );
    }
    let Ok(body) = json::parse(&String::from_utf8_lossy(&request.body)) else {
        return respond(
            &mut stream,
            400,
            "Bad Request",
            &error_body("the body is not JSON"),
        );
    };
    if body.get("stream") != Some(&Value::Bool(true)) {
        return respond(
            &mut stream,
            400,
            "Bad Request",
            &error_body("the mock only streams"),
        );
    }
    let tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .map_or(shared.cfg.default_tokens, |n| {
            u32::try_from(n).unwrap_or(u32::MAX).min(100_000)
        });
    let include_usage = body
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .is_some_and(|v| *v == Value::Bool(true));
    stream_tokens(&mut stream, shared, tokens, include_usage)
}

fn write_chunk(stream: &mut TcpStream, payload: &str) -> io::Result<()> {
    stream.write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes())
}

fn event(value: &Value) -> String {
    format!("data: {value}\n\n")
}

fn delta(content: Option<&str>, role: bool, finish: Option<&str>) -> Value {
    let mut fields = Vec::new();
    if role {
        fields.push(("role", "assistant".into()));
    }
    if let Some(text) = content {
        fields.push(("content", text.into()));
    }
    let delta = Value::Obj(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    );
    let choice = Value::obj([
        ("index", 0u64.into()),
        ("delta", delta),
        ("finish_reason", finish.map_or(Value::Null, Into::into)),
    ]);
    Value::obj([
        ("id", "mock-1".into()),
        ("object", "chat.completion.chunk".into()),
        ("model", "mock".into()),
        ("choices", Value::Arr(vec![choice])),
    ])
}

fn stream_tokens(
    stream: &mut TcpStream,
    shared: &Shared,
    tokens: u32,
    include_usage: bool,
) -> io::Result<()> {
    let _slot = shared.slots.acquire();
    shared.wait_out_stall();
    thread::sleep(shared.cfg.ttft);
    shared.wait_out_stall();
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
          Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
    )?;
    for i in 0..tokens {
        if i > 0 {
            thread::sleep(shared.cfg.itl);
            shared.wait_out_stall();
        }
        if shared.stop.load(Ordering::Relaxed) || shared.cfg.die_after_tokens == Some(i) {
            return Ok(()); // the connection just closes, mid-stream
        }
        let mut payload = String::new();
        if i == 0 {
            payload += &event(&delta(Some(""), true, None)); // the role chunk, before any text
        }
        payload += &event(&delta(Some(&format!("tok{i} ")), false, None));
        write_chunk(stream, &payload)?;
    }
    let mut tail = event(&delta(None, false, Some("stop")));
    if include_usage {
        let usage = Value::obj([
            ("prompt_tokens", 16u64.into()),
            ("completion_tokens", u64::from(tokens).into()),
            ("total_tokens", (16 + u64::from(tokens)).into()),
        ]);
        tail += &event(&Value::obj([
            ("choices", Value::Arr(vec![])),
            ("usage", usage),
        ]));
    }
    tail += "data: [DONE]\n\n";
    write_chunk(stream, &tail)?;
    stream.write_all(b"0\r\n\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{build_post, parse_url, Response};
    use crate::sse;

    struct Reply {
        status: u16,
        events: Vec<(Duration, String)>,
        total: Duration,
        error: Option<String>,
    }

    /// One request, with the time each event arrived.
    fn ask(server: &Server, body: &str) -> Reply {
        let url = parse_url(&server.url()).unwrap();
        let started = Instant::now();
        let mut stream = TcpStream::connect(server.addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(&build_post(&url, body.as_bytes(), &[]))
            .unwrap();
        let mut reply = Reply {
            status: 0,
            events: Vec::new(),
            total: Duration::ZERO,
            error: None,
        };
        let mut response = match Response::start(stream) {
            Ok(r) => r,
            Err(e) => {
                reply.error = Some(e.to_string());
                return reply;
            }
        };
        reply.status = response.head.status;
        let mut parser = sse::Parser::new();
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match response.read_body(&mut bytes) {
                Ok(more) => {
                    for e in parser.push(&bytes) {
                        reply.events.push((started.elapsed(), e.data));
                    }
                    if !more {
                        break;
                    }
                }
                Err(e) => {
                    reply.error = Some(e.to_string());
                    break;
                }
            }
        }
        reply.total = started.elapsed();
        reply
    }

    const STREAM: &str = r#"{"model":"m","messages":[],"stream":true,"max_tokens":5}"#;

    fn quick() -> MockConfig {
        MockConfig {
            ttft: Duration::from_millis(40),
            itl: Duration::from_millis(10),
            ..MockConfig::default()
        }
    }

    fn contents(reply: &Reply) -> Vec<String> {
        reply
            .events
            .iter()
            .filter_map(|(_, data)| json::parse(data).ok())
            .filter_map(|v| {
                let c = v
                    .get("choices")?
                    .at(0)?
                    .get("delta")?
                    .get("content")?
                    .as_str()?
                    .to_string();
                Some(c)
            })
            .collect()
    }

    #[test]
    fn it_streams_the_requested_number_of_tokens_then_done() {
        let server = Server::start("127.0.0.1:0", quick()).unwrap();
        let reply = ask(&server, STREAM);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.error, None);
        // the role chunk has empty content, then the tokens
        assert_eq!(
            contents(&reply),
            vec!["", "tok0 ", "tok1 ", "tok2 ", "tok3 ", "tok4 "]
        );
        assert_eq!(reply.events.last().map(|e| e.1.as_str()), Some("[DONE]"));
    }

    #[test]
    fn usage_is_sent_when_asked_for() {
        let server = Server::start("127.0.0.1:0", quick()).unwrap();
        let body = r#"{"stream":true,"max_tokens":3,"stream_options":{"include_usage":true}}"#;
        let reply = ask(&server, body);
        let usage = reply
            .events
            .iter()
            .filter_map(|(_, d)| json::parse(d).ok())
            .find_map(|v| {
                v.get("usage")
                    .and_then(|u| u.get("completion_tokens"))
                    .and_then(Value::as_u64)
            });
        assert_eq!(usage, Some(3));
        let without = ask(&server, STREAM);
        assert!(without.events.iter().all(|(_, d)| !d.contains("usage")));
    }

    #[test]
    fn the_default_number_of_tokens_applies_without_max_tokens() {
        let cfg = MockConfig {
            default_tokens: 4,
            ..quick()
        };
        let server = Server::start("127.0.0.1:0", cfg).unwrap();
        let reply = ask(&server, r#"{"stream":true}"#);
        assert_eq!(contents(&reply).len(), 1 + 4);
    }

    #[test]
    fn the_first_token_comes_after_the_ttft_and_the_rest_one_itl_apart() {
        let server = Server::start("127.0.0.1:0", quick()).unwrap();
        let reply = ask(&server, r#"{"stream":true,"max_tokens":8}"#);
        let first_text = reply
            .events
            .iter()
            .find(|(_, d)| d.contains("tok0"))
            .unwrap()
            .0;
        assert!(
            first_text >= Duration::from_millis(40),
            "first token after {first_text:?}"
        );
        assert!(
            first_text < Duration::from_millis(120),
            "first token after {first_text:?}"
        );
        // 8 tokens: 7 gaps of 10 ms after the first
        let spread = reply.total.saturating_sub(first_text);
        assert!(spread >= Duration::from_millis(70), "{spread:?}");
        assert!(spread < Duration::from_millis(200), "{spread:?}");
    }

    #[test]
    fn requests_beyond_the_slots_wait_their_turn() {
        let cfg = MockConfig {
            slots: 1,
            ttft: Duration::from_millis(100),
            itl: Duration::ZERO,
            ..quick()
        };
        let server = Server::start("127.0.0.1:0", cfg).unwrap();
        let started = Instant::now();
        let totals: Vec<Duration> = thread::scope(|s| {
            let handles: Vec<_> = (0..3)
                .map(|_| s.spawn(|| ask(&server, STREAM).total))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut sorted = totals.clone();
        sorted.sort();
        assert!(sorted[0] >= Duration::from_millis(100), "{sorted:?}");
        assert!(sorted[1] >= Duration::from_millis(200), "{sorted:?}");
        assert!(sorted[2] >= Duration::from_millis(300), "{sorted:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn with_enough_slots_requests_run_side_by_side() {
        let cfg = MockConfig {
            slots: 4,
            ttft: Duration::from_millis(100),
            itl: Duration::ZERO,
            ..quick()
        };
        let server = Server::start("127.0.0.1:0", cfg).unwrap();
        let started = Instant::now();
        thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| ask(&server, STREAM));
            }
        });
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_stall_freezes_new_requests_and_streams_in_progress() {
        let cfg = MockConfig {
            stall: Some(Stall {
                after: Duration::from_millis(100),
                length: Duration::from_millis(400),
            }),
            ttft: Duration::from_millis(10),
            itl: Duration::from_millis(10),
            ..MockConfig::default()
        };
        let server = Server::start("127.0.0.1:0", cfg).unwrap();
        let in_progress = thread::scope(|s| {
            let long = s.spawn(|| ask(&server, r#"{"stream":true,"max_tokens":30}"#));
            thread::sleep(Duration::from_millis(200)); // inside the stall
            let during = ask(&server, STREAM);
            (long.join().unwrap(), during)
        });
        let (long, during) = in_progress;
        // 30 tokens would take about 300 ms; the 400 ms freeze is added to it
        assert!(long.total >= Duration::from_millis(650), "{:?}", long.total);
        // a request sent at 200 ms cannot start before the freeze ends at 500 ms
        let first = during
            .events
            .iter()
            .find(|(_, d)| d.contains("tok0"))
            .unwrap()
            .0;
        assert!(
            first >= Duration::from_millis(250),
            "{first:?} after sending, inside the stall"
        );
    }

    #[test]
    fn an_error_status_can_be_forced() {
        let cfg = MockConfig {
            error_status: Some(503),
            ..quick()
        };
        let server = Server::start("127.0.0.1:0", cfg).unwrap();
        let reply = ask(&server, STREAM);
        assert_eq!(reply.status, 503);
    }

    #[test]
    fn a_connection_can_die_mid_stream() {
        let cfg = MockConfig {
            die_after_tokens: Some(2),
            ..quick()
        };
        let server = Server::start("127.0.0.1:0", cfg).unwrap();
        let reply = ask(&server, STREAM);
        assert_eq!(reply.status, 200);
        assert!(
            reply.error.as_deref().unwrap_or("").contains("closed"),
            "the client must notice"
        );
        assert_eq!(contents(&reply), vec!["", "tok0 ", "tok1 "]);
    }

    #[test]
    fn bad_requests_get_a_json_error() {
        let server = Server::start("127.0.0.1:0", quick()).unwrap();
        assert_eq!(ask(&server, "not json").status, 400);
        assert_eq!(ask(&server, r#"{"stream":false}"#).status, 400);
        assert_eq!(ask(&server, r#"{"max_tokens":3}"#).status, 400);
    }

    #[test]
    fn it_counts_what_it_served_and_stops_cleanly() {
        let mut server = Server::start("127.0.0.1:0", quick()).unwrap();
        for _ in 0..3 {
            ask(&server, STREAM);
        }
        thread::sleep(Duration::from_millis(50));
        assert_eq!(server.served(), 3);
        server.stop();
        assert!(
            TcpStream::connect(server.addr()).is_err(),
            "the port is closed after stop"
        );
    }
}
