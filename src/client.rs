//! One streaming request, with every latency measured from the moment it was meant to start.

use crate::http::{self, HttpError, Response, Url};
use crate::json::{self, Value};
use crate::rng::Rng;
use crate::sse;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct RequestSpec {
    pub url: Url,
    pub model: String,
    pub prompt: String,
    pub max_tokens: u32,
    /// For connecting, and for each wait on the connection (a stream that goes quiet for this
    /// long has failed).
    pub timeout: Duration,
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The server answered with this status instead of a stream.
    Status(u16),
    Timeout,
    /// Connection refused, reset, closed early, malformed stream, an error event...
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct Sample {
    pub outcome: Outcome,
    /// From the intended start to the first token with text in it.
    pub ttft: Option<Duration>,
    /// Gaps between the arrival of consecutive tokens. Tokens that arrive in one read share a
    /// timestamp, so batching on the way shows up as zeros.
    pub itl: Vec<Duration>,
    /// From the intended start to the end of the stream, or to the failure.
    pub e2e: Duration,
    /// Completion tokens: the server's own count when it sends one, else the events seen.
    pub tokens: u64,
    /// How late the request actually started compared with its intended start: the health of the
    /// generator itself, not of the server.
    pub lag: Duration,
}

/// A prompt of `words` pseudo-words, the same for the same seed.
pub fn prompt(words: usize, seed: u64) -> String {
    const SYLLABLES: [&str; 12] = [
        "ka", "lo", "mi", "ne", "ru", "sa", "to", "vi", "ze", "pa", "do", "li",
    ];
    let mut rng = Rng::new(seed);
    let mut out = String::new();
    for i in 0..words {
        if i > 0 {
            out.push(' ');
        }
        for _ in 0..=rng.below(3) {
            out.push_str(SYLLABLES[rng.below(SYLLABLES.len() as u64) as usize]);
        }
    }
    out
}

pub fn request_body(spec: &RequestSpec) -> String {
    let message = Value::obj([
        ("role", "user".into()),
        ("content", spec.prompt.as_str().into()),
    ]);
    Value::obj([
        ("model", spec.model.as_str().into()),
        ("messages", Value::Arr(vec![message])),
        ("max_tokens", u64::from(spec.max_tokens).into()),
        ("stream", true.into()),
        (
            "stream_options",
            Value::obj([("include_usage", true.into())]),
        ),
    ])
    .to_string()
}

/// Send one request and measure it. `intended` is when it was meant to start; if the caller got
/// to it late, the lateness counts against the latency, as it would for a real user.
pub fn run_one(spec: &RequestSpec, intended: Instant) -> Sample {
    let started = Instant::now();
    let mut sample = Sample {
        outcome: Outcome::Ok,
        ttft: None,
        itl: Vec::new(),
        e2e: Duration::ZERO,
        tokens: 0,
        lag: started.saturating_duration_since(intended),
    };
    sample.outcome = match stream(spec, intended, &mut sample) {
        Ok(None) => Outcome::Ok,
        Ok(Some(status)) => Outcome::Status(status),
        Err(Failure::Timeout) => Outcome::Timeout,
        Err(Failure::Other(why)) => Outcome::Failed(why),
    };
    sample.e2e = Instant::now().saturating_duration_since(intended);
    sample
}

enum Failure {
    Timeout,
    Other(String),
}

impl From<HttpError> for Failure {
    fn from(e: HttpError) -> Self {
        if e.is_timeout() {
            Failure::Timeout
        } else {
            Failure::Other(e.to_string())
        }
    }
}

fn connect(spec: &RequestSpec) -> Result<TcpStream, Failure> {
    let addr = spec
        .url
        .address()
        .to_socket_addrs()
        .map_err(|e| Failure::Other(format!("cannot resolve {}: {e}", spec.url.host)))?
        .next()
        .ok_or_else(|| Failure::Other(format!("no address for {}", spec.url.host)))?;
    let stream = TcpStream::connect_timeout(&addr, spec.timeout).map_err(|e| {
        if e.kind() == std::io::ErrorKind::TimedOut {
            Failure::Timeout
        } else {
            Failure::Other(format!("connect: {e}"))
        }
    })?;
    let io = |e: std::io::Error| Failure::Other(format!("socket setup: {e}"));
    stream.set_nodelay(true).map_err(io)?;
    stream.set_read_timeout(Some(spec.timeout)).map_err(io)?;
    stream.set_write_timeout(Some(spec.timeout)).map_err(io)?;
    Ok(stream)
}

/// `Ok(Some(status))` when the server answered with an error status instead of a stream.
fn stream(
    spec: &RequestSpec,
    intended: Instant,
    sample: &mut Sample,
) -> Result<Option<u16>, Failure> {
    let mut socket = connect(spec)?;
    let auth = spec.api_key.as_ref().map(|k| format!("Bearer {k}"));
    let headers: Vec<(&str, &str)> = auth.iter().map(|a| ("Authorization", a.as_str())).collect();
    let request = http::build_post(&spec.url, request_body(spec).as_bytes(), &headers);
    socket.write_all(&request).map_err(|e| {
        let e = HttpError::from(e);
        Failure::from(e)
    })?;
    let mut response = Response::start(socket)?;
    if response.head.status != 200 {
        return Ok(Some(response.head.status));
    }
    let mut parser = sse::Parser::new();
    let mut bytes = Vec::new();
    let mut last_token: Option<Instant> = None;
    let mut seen = 0u64;
    let mut reported = None;
    loop {
        bytes.clear();
        let more = response.read_body(&mut bytes)?;
        let now = Instant::now();
        for event in parser.push(&bytes) {
            if event.data == "[DONE]" {
                continue;
            }
            let Ok(chunk) = json::parse(&event.data) else {
                return Err(Failure::Other(format!(
                    "unreadable event: {:.80}",
                    event.data
                )));
            };
            if let Some(error) = chunk.get("error") {
                return Err(Failure::Other(format!("error event: {error}")));
            }
            if let Some(n) = chunk.get("usage").and_then(|u| u.get("completion_tokens")) {
                reported = n.as_u64().or(reported);
            }
            let text = chunk
                .get("choices")
                .and_then(|c| c.at(0))
                .and_then(|c| c.get("delta"))
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str);
            if text.is_some_and(|t| !t.is_empty()) {
                seen += 1;
                match last_token {
                    None => sample.ttft = Some(now.saturating_duration_since(intended)),
                    Some(before) => sample.itl.push(now.saturating_duration_since(before)),
                }
                last_token = Some(now);
                sample.tokens = seen;
            }
        }
        if !more {
            break;
        }
    }
    sample.tokens = reported.unwrap_or(seen);
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::parse_url;
    use crate::mock::{MockConfig, Server};
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    fn spec_for(url: &str) -> RequestSpec {
        RequestSpec {
            url: parse_url(url).unwrap(),
            model: "m".to_string(),
            prompt: "hello".to_string(),
            max_tokens: 5,
            timeout: Duration::from_secs(5),
            api_key: None,
        }
    }

    fn mock(ttft_ms: u64, itl_ms: u64) -> Server {
        let cfg = MockConfig {
            ttft: Duration::from_millis(ttft_ms),
            itl: Duration::from_millis(itl_ms),
            ..MockConfig::default()
        };
        Server::start("127.0.0.1:0", cfg).unwrap()
    }

    /// A server that reads the request and then plays a script on the connection.
    fn scripted(script: fn(&mut TcpStream)) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            script(&mut stream);
        });
        (url, handle)
    }

    const HEAD: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

    #[test]
    fn a_normal_stream_is_measured() {
        let server = mock(60, 15);
        let sample = run_one(&spec_for(&server.url()), Instant::now());
        assert_eq!(sample.outcome, Outcome::Ok);
        assert_eq!(sample.tokens, 5);
        let ttft = sample.ttft.unwrap();
        assert!(
            ttft >= Duration::from_millis(60) && ttft < Duration::from_secs(1),
            "{ttft:?}"
        );
        assert_eq!(sample.itl.len(), 4);
        for gap in &sample.itl {
            assert!(
                *gap >= Duration::from_millis(12) && *gap < Duration::from_millis(500),
                "{gap:?}"
            );
        }
        assert!(
            sample.e2e >= ttft + Duration::from_millis(50),
            "{:?}",
            sample.e2e
        );
        assert!(sample.lag < Duration::from_millis(500));
    }

    #[test]
    fn latencies_count_from_the_intended_start_not_from_the_send() {
        let server = mock(20, 5);
        let intended = Instant::now()
            .checked_sub(Duration::from_millis(300))
            .unwrap();
        let sample = run_one(&spec_for(&server.url()), intended);
        assert!(sample.lag >= Duration::from_millis(300), "{:?}", sample.lag);
        assert!(
            sample.ttft.unwrap() >= Duration::from_millis(320),
            "{:?}",
            sample.ttft
        );
        assert!(sample.e2e >= Duration::from_millis(340), "{:?}", sample.e2e);
    }

    #[test]
    fn an_intended_start_in_the_future_does_not_go_negative() {
        let server = mock(5, 1);
        let intended = Instant::now() + Duration::from_secs(30);
        let sample = run_one(&spec_for(&server.url()), intended);
        assert_eq!(sample.outcome, Outcome::Ok, "{sample:?}");
        assert_eq!(sample.lag, Duration::ZERO);
        assert_eq!(sample.ttft, Some(Duration::ZERO));
        assert_eq!(sample.outcome, Outcome::Ok);
    }

    #[test]
    fn the_role_chunk_with_empty_text_is_not_the_first_token() {
        let (url, server) = scripted(|stream| {
            stream.write_all(HEAD).unwrap();
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n")
                .unwrap();
            thread::sleep(Duration::from_millis(150));
            stream
                .write_all(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
                )
                .unwrap();
        });
        let sample = run_one(&spec_for(&url), Instant::now());
        server.join().unwrap();
        assert_eq!(sample.outcome, Outcome::Ok);
        assert!(
            sample.ttft.unwrap() >= Duration::from_millis(150),
            "{:?}",
            sample.ttft
        );
        assert_eq!(sample.tokens, 1);
    }

    #[test]
    fn without_a_usage_chunk_the_tokens_are_counted() {
        let (url, server) = scripted(|stream| {
            stream.write_all(HEAD).unwrap();
            for t in ["a", "b", "c"] {
                let event =
                    format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{t}\"}}}}]}}\n\n");
                stream.write_all(event.as_bytes()).unwrap();
                thread::sleep(Duration::from_millis(20));
            }
            stream.write_all(b"data: [DONE]\n\n").unwrap();
        });
        let sample = run_one(&spec_for(&url), Instant::now());
        server.join().unwrap();
        assert_eq!(
            (sample.outcome, sample.tokens, sample.itl.len()),
            (Outcome::Ok, 3, 2)
        );
    }

    #[test]
    fn the_servers_own_token_count_wins() {
        let (url, server) = scripted(|stream| {
            stream.write_all(HEAD).unwrap();
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"a b c\"}}]}\n\n")
                .unwrap();
            stream.write_all(b"data: {\"choices\":[],\"usage\":{\"completion_tokens\":3}}\n\ndata: [DONE]\n\n").unwrap();
        });
        let sample = run_one(&spec_for(&url), Instant::now());
        server.join().unwrap();
        assert_eq!(sample.tokens, 3);
    }

    #[test]
    fn an_error_status_is_an_outcome_not_a_stream() {
        let cfg = MockConfig {
            error_status: Some(503),
            ..MockConfig::default()
        };
        let server = Server::start("127.0.0.1:0", cfg).unwrap();
        let sample = run_one(&spec_for(&server.url()), Instant::now());
        assert_eq!(sample.outcome, Outcome::Status(503));
        assert_eq!((sample.tokens, sample.ttft), (0, None));
    }

    #[test]
    fn a_connection_that_dies_mid_stream_is_a_failure_with_what_arrived_before() {
        let cfg = MockConfig {
            die_after_tokens: Some(3),
            ttft: Duration::from_millis(5),
            itl: Duration::from_millis(5),
            ..MockConfig::default()
        };
        let server = Server::start("127.0.0.1:0", cfg).unwrap();
        let sample = run_one(&spec_for(&server.url()), Instant::now());
        assert!(
            matches!(&sample.outcome, Outcome::Failed(why) if why.contains("closed")),
            "{:?}",
            sample.outcome
        );
        assert_eq!(sample.tokens, 3);
        assert!(sample.ttft.is_some());
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        let (url, server) = scripted(|_| thread::sleep(Duration::from_millis(600)));
        let mut spec = spec_for(&url);
        spec.timeout = Duration::from_millis(150);
        let sample = run_one(&spec, Instant::now());
        server.join().unwrap();
        assert_eq!(sample.outcome, Outcome::Timeout);
        assert!(
            sample.e2e >= Duration::from_millis(150) && sample.e2e < Duration::from_millis(500)
        );
    }

    #[test]
    fn a_stream_that_goes_quiet_times_out_after_its_first_tokens() {
        let (url, server) = scripted(|stream| {
            stream.write_all(HEAD).unwrap();
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n")
                .unwrap();
            thread::sleep(Duration::from_millis(600));
        });
        let mut spec = spec_for(&url);
        spec.timeout = Duration::from_millis(150);
        let sample = run_one(&spec, Instant::now());
        server.join().unwrap();
        assert_eq!(sample.outcome, Outcome::Timeout);
        assert_eq!(sample.tokens, 1);
    }

    #[test]
    fn a_refused_connection_is_a_failure() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let sample = run_one(
            &spec_for(&format!("http://127.0.0.1:{port}/")),
            Instant::now(),
        );
        assert!(
            matches!(&sample.outcome, Outcome::Failed(why) if why.starts_with("connect")),
            "{:?}",
            sample.outcome
        );
        assert_eq!(sample.tokens, 0);
    }

    #[test]
    fn an_error_event_in_the_stream_is_a_failure() {
        let (url, server) = scripted(|stream| {
            stream.write_all(HEAD).unwrap();
            stream
                .write_all(b"data: {\"error\":{\"message\":\"out of memory\"}}\n\n")
                .unwrap();
        });
        let sample = run_one(&spec_for(&url), Instant::now());
        server.join().unwrap();
        assert!(matches!(&sample.outcome, Outcome::Failed(why) if why.contains("out of memory")));
    }

    #[test]
    fn garbage_in_the_stream_is_a_failure() {
        let (url, server) = scripted(|stream| {
            stream.write_all(HEAD).unwrap();
            stream.write_all(b"data: <html>oops</html>\n\n").unwrap();
        });
        let sample = run_one(&spec_for(&url), Instant::now());
        server.join().unwrap();
        assert!(matches!(&sample.outcome, Outcome::Failed(why) if why.contains("unreadable")));
    }

    #[test]
    fn the_api_key_is_sent_as_a_bearer_token() {
        let (url, server) = scripted(|stream| {
            // the script cannot see the request here, so it only answers; the check is below
            stream.write_all(HEAD).unwrap();
            stream.write_all(b"data: [DONE]\n\n").unwrap();
        });
        let mut spec = spec_for(&url);
        spec.api_key = Some("sk-test".to_string());
        let auth = spec
            .api_key
            .as_ref()
            .map(|k| format!("Bearer {k}"))
            .unwrap();
        let request = http::build_post(&spec.url, b"{}", &[("Authorization", &auth)]);
        assert!(String::from_utf8(request)
            .unwrap()
            .contains("Authorization: Bearer sk-test\r\n"));
        assert_eq!(run_one(&spec, Instant::now()).outcome, Outcome::Ok);
        server.join().unwrap();
    }

    #[test]
    fn the_body_is_valid_json_with_what_a_server_expects() {
        let mut spec = spec_for("http://h/");
        spec.prompt = "say \"hi\"\nthen \u{e9}\u{1f600}".to_string();
        spec.max_tokens = 64;
        let body = json::parse(&request_body(&spec)).unwrap();
        let content = body
            .get("messages")
            .and_then(|m| m.at(0))
            .and_then(|m| m.get("content"));
        assert_eq!(content.and_then(Value::as_str), Some(spec.prompt.as_str()));
        assert_eq!(body.get("max_tokens").and_then(Value::as_u64), Some(64));
        assert_eq!(body.get("stream"), Some(&Value::Bool(true)));
        assert_eq!(body.get("model").and_then(Value::as_str), Some("m"));
        let usage = body
            .get("stream_options")
            .and_then(|o| o.get("include_usage"));
        assert_eq!(usage, Some(&Value::Bool(true)));
    }

    #[test]
    fn prompts_have_the_requested_words_and_are_reproducible() {
        for words in [0, 1, 7, 100] {
            let text = prompt(words, 5);
            assert_eq!(text.split_whitespace().count(), words);
            assert_eq!(text, prompt(words, 5));
        }
        assert_ne!(prompt(20, 1), prompt(20, 2));
    }
}
