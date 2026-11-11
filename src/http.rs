//! Just enough HTTP/1.1 for a streaming client: build a POST, read the response head, then read
//! the body as it arrives, whether it is chunked, has a length, or runs until the connection
//! closes. There is no TLS: point it at an `http://` endpoint.

use std::fmt::{self, Write as _};
use std::io::{self, Read};

const MAX_HEAD: usize = 64 * 1024;
const MAX_CHUNK_LINE: usize = 1024;

#[derive(Debug)]
pub enum HttpError {
    Io(io::Error),
    Protocol(String),
}

impl HttpError {
    fn protocol(what: &str) -> Self {
        HttpError::Protocol(what.to_string())
    }

    /// A read that hit the socket timeout.
    pub fn is_timeout(&self) -> bool {
        matches!(self, HttpError::Io(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Io(e) => write!(f, "i/o error: {e}"),
            HttpError::Protocol(s) => write!(f, "protocol error: {s}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<io::Error> for HttpError {
    fn from(e: io::Error) -> Self {
        HttpError::Io(e)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(text: &str) -> Result<Url, String> {
    if text.starts_with("https://") {
        return Err(
            "https is not supported, there is no TLS: use an http:// endpoint or a \
                    TLS-terminating proxy in front of the server"
                .to_string(),
        );
    }
    let Some(rest) = text.strip_prefix("http://") else {
        return Err(format!(
            "expected a URL starting with http://, got {text:?}"
        ));
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or("unterminated [ in the host")?;
        (host, after.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err(format!("no host in {text:?}"));
    }
    let port = match port {
        Some(p) => p.parse::<u16>().map_err(|_| format!("bad port {p:?}"))?,
        None => 80,
    };
    Ok(Url {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

impl Url {
    /// `host:port`, as `TcpStream::connect` wants it.
    pub fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// A complete `POST` with a JSON body. The server is asked to close the connection after the
/// response, so one request is one connection and its end is unambiguous.
pub fn build_post(url: &Url, body: &[u8], extra_headers: &[(&str, &str)]) -> Vec<u8> {
    let host = if url.port == 80 {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let mut head = format!(
        "POST {} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Accept: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        url.path,
        body.len()
    );
    for (name, value) in extra_headers {
        let _ = write!(head, "{name}: {value}\r\n"); // writing to a String cannot fail
    }
    head.push_str("\r\n");
    let mut request = head.into_bytes();
    request.extend_from_slice(body);
    request
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Parse a response head from the start of `buf`. `Ok(None)` means the blank line that ends the
/// head has not arrived yet; otherwise the head and the number of bytes it took.
pub fn parse_head(buf: &[u8]) -> Result<Option<(Head, usize)>, HttpError> {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEAD {
            Err(HttpError::protocol("response head too large"))
        } else {
            Ok(None)
        };
    };
    let text = std::str::from_utf8(&buf[..end])
        .map_err(|_| HttpError::protocol("response head is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Protocol(format!(
            "not an HTTP/1.x response: {status_line:?}"
        )));
    }
    let status = parts
        .next()
        .filter(|s| s.len() == 3)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Protocol(format!("bad status line {status_line:?}")))?;
    let reason = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| HttpError::Protocol(format!("bad header line {line:?}")))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(Some((
        Head {
            status,
            reason,
            headers,
        },
        end + 4,
    )))
}

#[derive(Debug)]
enum ChunkState {
    Size,
    Data(u64),
    DataEnd,
    Trailer,
    Done,
}

/// Decodes `Transfer-Encoding: chunked` as the bytes come, cut anywhere.
#[derive(Debug)]
pub struct ChunkedDecoder {
    state: ChunkState,
    line: Vec<u8>,
}

impl Default for ChunkedDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkedDecoder {
    pub fn new() -> Self {
        Self {
            state: ChunkState::Size,
            line: Vec::new(),
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(self.state, ChunkState::Done)
    }

    /// Decode `input`, appending the body bytes to `out`. Returns how many bytes of `input` were
    /// used: all of them, unless the body ended inside it.
    pub fn push(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize, HttpError> {
        let mut i = 0;
        while i < input.len() {
            match self.state {
                ChunkState::Done => break,
                ChunkState::Size | ChunkState::Trailer => {
                    let byte = input[i];
                    i += 1;
                    if byte != b'\n' {
                        self.line.push(byte);
                        if self.line.len() > MAX_CHUNK_LINE {
                            return Err(HttpError::protocol("chunk header line too long"));
                        }
                        continue;
                    }
                    if self.line.last() == Some(&b'\r') {
                        self.line.pop();
                    }
                    let line = std::mem::take(&mut self.line);
                    self.state = if matches!(self.state, ChunkState::Trailer) {
                        if line.is_empty() {
                            ChunkState::Done
                        } else {
                            ChunkState::Trailer // a trailer header: ignored
                        }
                    } else {
                        match chunk_size(&line)? {
                            0 => ChunkState::Trailer,
                            n => ChunkState::Data(n),
                        }
                    };
                }
                ChunkState::Data(left) => {
                    let take = usize::try_from(left)
                        .unwrap_or(usize::MAX)
                        .min(input.len() - i);
                    out.extend_from_slice(&input[i..i + take]);
                    i += take;
                    let left = left - take as u64;
                    self.state = if left == 0 {
                        ChunkState::DataEnd
                    } else {
                        ChunkState::Data(left)
                    };
                }
                ChunkState::DataEnd => {
                    let byte = input[i];
                    i += 1;
                    match byte {
                        b'\r' => {}
                        b'\n' => self.state = ChunkState::Size,
                        _ => return Err(HttpError::protocol("missing CRLF after chunk data")),
                    }
                }
            }
        }
        Ok(i)
    }
}

fn chunk_size(line: &[u8]) -> Result<u64, HttpError> {
    let text = std::str::from_utf8(line).map_err(|_| HttpError::protocol("bad chunk size"))?;
    let digits = text.split(';').next().unwrap_or("").trim();
    if digits.is_empty() || digits.len() > 16 {
        return Err(HttpError::Protocol(format!("bad chunk size {text:?}")));
    }
    u64::from_str_radix(digits, 16)
        .map_err(|_| HttpError::Protocol(format!("bad chunk size {text:?}")))
}

#[derive(Debug)]
enum Body {
    Length(u64),
    Chunked(ChunkedDecoder),
    UntilClose,
    Done,
}

/// A response whose head has been read; the body is read piece by piece.
pub struct Response<R: Read> {
    pub head: Head,
    reader: R,
    body: Body,
    carry: Vec<u8>,
    scratch: Vec<u8>,
}

impl<R: Read> Response<R> {
    /// Read until the head is complete.
    pub fn start(mut reader: R) -> Result<Self, HttpError> {
        let mut seen = Vec::new();
        let mut scratch = vec![0u8; 16 * 1024];
        loop {
            if let Some((head, used)) = parse_head(&seen)? {
                let body = framing(&head)?;
                let carry = seen.split_off(used);
                return Ok(Self {
                    head,
                    reader,
                    body,
                    carry,
                    scratch,
                });
            }
            let n = reader.read(&mut scratch)?;
            if n == 0 {
                return Err(HttpError::protocol(
                    "connection closed before the response head",
                ));
            }
            seen.extend_from_slice(&scratch[..n]);
        }
    }

    /// Wait for the next bytes of the body and append them to `out`. One call reads at most once
    /// from the connection, so the time it returns is the time the bytes arrived. Returns `false`
    /// once the body is complete.
    pub fn read_body(&mut self, out: &mut Vec<u8>) -> Result<bool, HttpError> {
        if matches!(self.body, Body::Done) {
            return Ok(false);
        }
        let (input, eof) = if self.carry.is_empty() {
            let n = self.reader.read(&mut self.scratch)?;
            (self.scratch[..n].to_vec(), n == 0)
        } else {
            (std::mem::take(&mut self.carry), false)
        };
        match &mut self.body {
            Body::Length(left) => {
                if eof {
                    return Err(HttpError::protocol(
                        "connection closed before content-length bytes",
                    ));
                }
                let take = usize::try_from(*left)
                    .unwrap_or(usize::MAX)
                    .min(input.len());
                out.extend_from_slice(&input[..take]);
                *left -= take as u64;
                if *left == 0 {
                    self.body = Body::Done;
                }
            }
            Body::Chunked(decoder) => {
                if eof {
                    return Err(HttpError::protocol(
                        "connection closed inside a chunked body",
                    ));
                }
                decoder.push(&input, out)?;
                if decoder.is_done() {
                    self.body = Body::Done;
                }
            }
            Body::UntilClose => {
                if eof {
                    self.body = Body::Done;
                } else {
                    out.extend_from_slice(&input);
                }
            }
            Body::Done => {}
        }
        Ok(!matches!(self.body, Body::Done))
    }
}

fn framing(head: &Head) -> Result<Body, HttpError> {
    if head.status < 200 || head.status == 204 || head.status == 304 {
        return Ok(Body::Done);
    }
    if head
        .header("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        return Ok(Body::Chunked(ChunkedDecoder::new()));
    }
    match head.header("content-length") {
        Some(v) => {
            let n = v
                .parse::<u64>()
                .map_err(|_| HttpError::Protocol(format!("bad content-length {v:?}")))?;
            Ok(if n == 0 { Body::Done } else { Body::Length(n) })
        }
        None => Ok(Body::UntilClose),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out its bytes `step` at a time, like a network with small packets.
    struct Trickle {
        data: Vec<u8>,
        pos: usize,
        step: usize,
    }

    impl Trickle {
        fn new(data: &[u8], step: usize) -> Self {
            Self {
                data: data.to_vec(),
                pos: 0,
                step,
            }
        }
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.step.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn body_of(response: &[u8], step: usize) -> Result<Vec<u8>, HttpError> {
        let mut r = Response::start(Trickle::new(response, step))?;
        let mut out = Vec::new();
        while r.read_body(&mut out)? {}
        Ok(out)
    }

    // --- urls and requests ---

    #[test]
    fn urls() {
        let u = parse_url("http://localhost:8000/v1/chat/completions").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("localhost", 8000, "/v1/chat/completions")
        );
        let u = parse_url("http://example.com").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("example.com", 80, "/")
        );
        let u = parse_url("http://10.0.0.1:9/x?y=1").unwrap();
        assert_eq!((u.port, u.path.as_str()), (9, "/x?y=1"));
        let u = parse_url("http://[::1]:8080/p").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 8080));
        assert_eq!(u.address(), "[::1]:8080");
        assert_eq!(parse_url("http://h:1/").unwrap().address(), "h:1");
    }

    #[test]
    fn bad_urls_are_explained() {
        assert!(parse_url("https://x/").unwrap_err().contains("no TLS"));
        assert!(parse_url("ftp://x/").unwrap_err().contains("http://"));
        assert!(parse_url("http:///path").unwrap_err().contains("no host"));
        assert!(parse_url("http://h:99999/")
            .unwrap_err()
            .contains("bad port"));
        assert!(parse_url("http://h:abc/").unwrap_err().contains("bad port"));
        assert!(parse_url("http://[::1/")
            .unwrap_err()
            .contains("unterminated"));
    }

    #[test]
    fn a_post_has_the_headers_a_server_needs() {
        let url = parse_url("http://h:8000/v1/x").unwrap();
        let request = build_post(&url, b"{\"a\":1}", &[("Authorization", "Bearer k")]);
        let text = String::from_utf8(request).unwrap();
        assert!(text.starts_with("POST /v1/x HTTP/1.1\r\nHost: h:8000\r\n"));
        assert!(text.contains("\r\nContent-Length: 7\r\n"));
        assert!(text.contains("\r\nConnection: close\r\n"));
        assert!(text.contains("\r\nAuthorization: Bearer k\r\n"));
        assert!(text.ends_with("\r\n\r\n{\"a\":1}"));
        let default_port = build_post(&parse_url("http://h/").unwrap(), b"", &[]);
        assert!(String::from_utf8(default_port)
            .unwrap()
            .contains("Host: h\r\n"));
    }

    // --- the head ---

    #[test]
    fn a_complete_head_is_parsed_and_its_length_returned() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-A:  b c \r\n\r\nBODY";
        let (head, used) = parse_head(raw).unwrap().unwrap();
        assert_eq!((head.status, head.reason.as_str()), (200, "OK"));
        assert_eq!(head.header("content-type"), Some("text/event-stream"));
        assert_eq!(head.header("X-A"), Some("b c"));
        assert_eq!(&raw[used..], b"BODY");
    }

    #[test]
    fn an_incomplete_head_asks_for_more() {
        let raw = b"HTTP/1.1 200 OK\r\nA: b\r\n\r\n";
        for cut in 0..raw.len() {
            assert!(parse_head(&raw[..cut]).unwrap().is_none(), "cut {cut}");
        }
    }

    #[test]
    fn a_reason_phrase_is_optional_and_may_contain_spaces() {
        let (head, _) = parse_head(b"HTTP/1.1 503\r\n\r\n").unwrap().unwrap();
        assert_eq!((head.status, head.reason.as_str()), (503, ""));
        let (head, _) = parse_head(b"HTTP/1.0 404 Not Found Here\r\n\r\n")
            .unwrap()
            .unwrap();
        assert_eq!(head.reason, "Not Found Here");
    }

    #[test]
    fn bad_heads_are_errors() {
        for raw in [
            &b"garbage\r\n\r\n"[..],
            b"HTTP/2 200 OK\r\n\r\n",
            b"HTTP/1.1 20 OK\r\n\r\n",
            b"HTTP/1.1 abc OK\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n\xff: x\r\n\r\n",
        ] {
            assert!(parse_head(raw).is_err(), "{raw:?}");
        }
        let huge = vec![b'a'; MAX_HEAD + 1];
        assert!(parse_head(&huge).is_err());
    }

    // --- chunked bodies ---

    const CHUNKED: &[u8] =
        b"5\r\nhello\r\n6;ext=1\r\n wor\r\n\r\nA\r\n0123456789\r\n0\r\nTrailer: x\r\n\r\n";
    const CHUNKED_BODY: &[u8] = b"hello wor\r\n0123456789";

    #[test]
    fn a_chunked_body_is_decoded() {
        let mut d = ChunkedDecoder::new();
        let mut out = Vec::new();
        assert_eq!(d.push(CHUNKED, &mut out).unwrap(), CHUNKED.len());
        assert!(d.is_done());
        assert_eq!(out, CHUNKED_BODY);
    }

    #[test]
    fn it_does_not_matter_where_the_bytes_are_cut() {
        for cut in 0..=CHUNKED.len() {
            let mut d = ChunkedDecoder::new();
            let mut out = Vec::new();
            let (a, b) = CHUNKED.split_at(cut);
            d.push(a, &mut out).unwrap();
            d.push(b, &mut out).unwrap();
            assert!(d.is_done(), "cut {cut}");
            assert_eq!(out, CHUNKED_BODY, "cut {cut}");
        }
        let mut d = ChunkedDecoder::new();
        let mut out = Vec::new();
        for byte in CHUNKED.chunks(1) {
            d.push(byte, &mut out).unwrap();
        }
        assert_eq!(out, CHUNKED_BODY);
    }

    #[test]
    fn data_that_looks_like_the_end_marker_is_just_data() {
        let mut d = ChunkedDecoder::new();
        let mut out = Vec::new();
        d.push(b"8\r\n\r\n0\r\n\r\nx\r\n0\r\n\r\n", &mut out)
            .unwrap();
        assert!(d.is_done());
        assert_eq!(out, b"\r\n0\r\n\r\nx");
    }

    #[test]
    fn hex_sizes_may_be_upper_case_and_the_last_chunk_needs_no_trailer_lines() {
        let mut d = ChunkedDecoder::new();
        let mut out = Vec::new();
        d.push(b"a\r\n0123456789\r\n0\r\n\r\n", &mut out).unwrap();
        assert!(d.is_done());
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn bytes_after_the_end_are_left_alone() {
        let mut d = ChunkedDecoder::new();
        let mut out = Vec::new();
        let input = b"1\r\nx\r\n0\r\n\r\nEXTRA";
        let used = d.push(input, &mut out).unwrap();
        assert_eq!(&input[used..], b"EXTRA");
        assert_eq!(d.push(b"more", &mut out).unwrap(), 0);
        assert_eq!(out, b"x");
    }

    #[test]
    fn broken_chunking_is_an_error() {
        for bad in [
            &b"zz\r\n"[..],
            b"\r\n",
            b"fffffffffffffffff\r\n",
            b"3\r\nabcX",
        ] {
            let mut d = ChunkedDecoder::new();
            assert!(d.push(bad, &mut Vec::new()).is_err(), "{bad:?}");
        }
        let mut d = ChunkedDecoder::new();
        assert!(d
            .push(&vec![b'1'; MAX_CHUNK_LINE + 1], &mut Vec::new())
            .is_err());
    }

    // --- whole responses ---

    fn chunked_response() -> Vec<u8> {
        let mut r = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        r.extend_from_slice(CHUNKED);
        r
    }

    #[test]
    fn a_chunked_response_arrives_whatever_the_packet_size() {
        let raw = chunked_response();
        for step in [1, 2, 3, 7, 16, 1000] {
            assert_eq!(body_of(&raw, step).unwrap(), CHUNKED_BODY, "step {step}");
        }
    }

    #[test]
    fn a_response_with_a_length() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        for step in [1, 4, 100] {
            assert_eq!(body_of(raw, step).unwrap(), b"hello", "step {step}");
        }
    }

    #[test]
    fn a_response_that_runs_until_the_connection_closes() {
        let raw =
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: 1\n\ndata: 2\n\n";
        for step in [1, 5, 100] {
            assert_eq!(
                body_of(raw, step).unwrap(),
                b"data: 1\n\ndata: 2\n\n",
                "step {step}"
            );
        }
    }

    #[test]
    fn responses_without_a_body() {
        for raw in [
            &b"HTTP/1.1 204 No Content\r\n\r\n"[..],
            b"HTTP/1.1 304 Not Modified\r\nContent-Length: 10\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        ] {
            assert_eq!(body_of(raw, 3).unwrap(), b"", "{raw:?}");
        }
    }

    #[test]
    fn the_body_bytes_that_came_with_the_head_are_not_lost() {
        let mut r = Response::start(Trickle::new(&chunked_response(), 10_000)).unwrap();
        let mut out = Vec::new();
        assert!(
            !r.read_body(&mut out).unwrap(),
            "the whole body was already there"
        );
        assert_eq!(out, CHUNKED_BODY);
        assert!(
            !r.read_body(&mut out).unwrap(),
            "a finished body stays finished"
        );
    }

    #[test]
    fn one_read_body_call_reads_the_socket_at_most_once() {
        let raw = chunked_response();
        let mut r = Response::start(Trickle::new(&raw, 4)).unwrap();
        let mut calls = 0;
        let mut out = Vec::new();
        while r.read_body(&mut out).unwrap() {
            calls += 1;
        }
        assert!(
            calls > 3,
            "the body should arrive in several pieces, got {calls}"
        );
    }

    #[test]
    fn a_connection_that_closes_too_early_is_an_error() {
        let cases: [&[u8]; 4] = [
            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Le",
        ];
        for raw in cases {
            assert!(
                body_of(raw, 3).is_err(),
                "{:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }

    #[test]
    fn a_bad_content_length_is_an_error() {
        assert!(body_of(b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n", 5).is_err());
    }

    #[test]
    fn timeouts_are_recognised() {
        let timeout = HttpError::Io(io::Error::new(io::ErrorKind::TimedOut, "slow"));
        let would_block = HttpError::Io(io::Error::from(io::ErrorKind::WouldBlock));
        assert!(timeout.is_timeout() && would_block.is_timeout());
        assert!(!HttpError::protocol("x").is_timeout());
        assert!(!HttpError::Io(io::Error::from(io::ErrorKind::BrokenPipe)).is_timeout());
        assert!(timeout.to_string().contains("slow"));
    }
}
