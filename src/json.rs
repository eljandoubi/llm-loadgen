//! A small JSON reader and writer: enough for chat-completion chunks and for the report.

use std::fmt::{self, Write};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    pub at: usize,
    pub what: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.at, self.what)
    }
}

impl std::error::Error for ParseError {}

const MAX_DEPTH: usize = 64;

pub fn parse(text: &str) -> Result<Value, ParseError> {
    let mut p = Parser {
        src: text.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let value = p.value(0)?;
    p.skip_ws();
    if p.pos != p.src.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(value)
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, what: &'static str) -> ParseError {
        ParseError { at: self.pos, what }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, ParseError> {
        if self.src[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.err("nested too deeply"));
        }
        match self.peek() {
            None => Err(self.err("unexpected end")),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Arr(items));
                }
                _ => return Err(self.err("expected a comma or ]")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.pos += 1;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Obj(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected a string key"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected a colon"));
            }
            self.pos += 1;
            self.skip_ws();
            members.push((key, self.value(depth + 1)?));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Obj(members));
                }
                _ => return Err(self.err("expected a comma or }")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // the cut points are ASCII, so the run is whole UTF-8 characters
            out.push_str(
                std::str::from_utf8(&self.src[start..self.pos])
                    .map_err(|_| self.err("bad utf-8"))?,
            );
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let c = self.peek().ok_or_else(|| self.err("unterminated string"))?;
                    self.pos += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.err("bad escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .src
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.err("short \\u escape"))?;
        let mut n = 0;
        for &d in digits {
            n = n * 16
                + char::from(d)
                    .to_digit(16)
                    .ok_or_else(|| self.err("bad \\u escape"))?;
        }
        self.pos += 4;
        Ok(n)
    }

    fn unicode_escape(&mut self) -> Result<char, ParseError> {
        let first = self.hex4()?;
        if (0xD800..0xDC00).contains(&first) {
            if self.src[self.pos..].starts_with(b"\\u") {
                self.pos += 2;
                let second = self.hex4()?;
                if (0xDC00..0xE000).contains(&second) {
                    let code = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                    return char::from_u32(code).ok_or_else(|| self.err("bad surrogate pair"));
                }
            }
            return Err(self.err("lone surrogate"));
        }
        char::from_u32(first).ok_or_else(|| self.err("lone surrogate"))
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.err("bad number")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.err("bad number"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.err("bad number"));
            }
        }
        let text =
            std::str::from_utf8(&self.src[start..self.pos]).map_err(|_| self.err("bad number"))?;
        text.parse::<f64>()
            .map(Value::Num)
            .map_err(|_| self.err("bad number"))
    }
}

impl Value {
    /// The member `key` of an object. With duplicate keys the last one wins, as in most parsers.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(members) => members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn at(&self, index: usize) -> Option<&Value> {
        match self {
            Value::Arr(items) => items.get(index),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// A whole, non-negative number.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 1.8e19 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn obj<const N: usize>(members: [(&str, Value); N]) -> Value {
        Value::Obj(
            members
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Num(n)
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Value::Num(n as f64)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

fn write_string(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    f.write_char('"')?;
    for c in s.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            c if u32::from(c) < 0x20 => write!(f, "\\u{:04x}", u32::from(c))?,
            c => f.write_char(c)?,
        }
    }
    f.write_char('"')
}

impl fmt::Display for Value {
    /// Compact JSON. Numbers that are not finite have no JSON form and are written as `null`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Num(n) if !n.is_finite() => f.write_str("null"),
            Value::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => write!(f, "{}", *n as i64),
            Value::Num(n) => write!(f, "{n}"),
            Value::Str(s) => write_string(f, s),
            Value::Arr(items) => {
                f.write_char('[')?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_char(']')
            }
            Value::Obj(members) => {
                f.write_char('{')?;
                for (i, (key, value)) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write_string(f, key)?;
                    write!(f, ":{value}")?;
                }
                f.write_char('}')
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    fn ok(text: &str) -> Value {
        parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
    }

    #[test]
    fn scalars() {
        assert_eq!(ok("null"), Value::Null);
        assert_eq!(ok(" true "), Value::Bool(true));
        assert_eq!(ok("false"), Value::Bool(false));
        assert_eq!(ok("0"), Value::Num(0.0));
        assert_eq!(ok("-12"), Value::Num(-12.0));
        assert_eq!(ok("3.25"), Value::Num(3.25));
        assert_eq!(ok("1e3"), Value::Num(1000.0));
        assert_eq!(ok("-2.5E-1"), Value::Num(-0.25));
        assert_eq!(ok("\"hi\""), Value::from("hi"));
    }

    #[test]
    fn string_escapes() {
        assert_eq!(
            ok(r#""a\"b\\c\/d\n\r\t\b\f""#),
            Value::from("a\"b\\c/d\n\r\t\u{8}\u{c}")
        );
        assert_eq!(ok(r#""\u00e9\u0041""#), Value::from("\u{e9}A"));
        assert_eq!(ok(r#""\ud83d\ude00""#), Value::from("\u{1f600}"));
        assert_eq!(
            ok("\"h\u{e9}llo \u{1f600}\""),
            Value::from("h\u{e9}llo \u{1f600}")
        );
    }

    #[test]
    fn a_chat_completion_chunk() {
        let chunk = ok(
            r#"{"id":"c-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":" Hel"},"finish_reason":null}]}"#,
        );
        let content = chunk
            .get("choices")
            .and_then(|c| c.at(0))
            .and_then(|c| c.get("delta"));
        assert_eq!(
            content
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str),
            Some(" Hel")
        );
        assert_eq!(chunk.get("missing"), None);
        assert_eq!(chunk.at(0), None);
    }

    #[test]
    fn a_usage_chunk() {
        let chunk = ok(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":128}}"#);
        let tokens = chunk.get("usage").and_then(|u| u.get("completion_tokens"));
        assert_eq!(tokens.and_then(Value::as_u64), Some(128));
    }

    #[test]
    fn as_u64_wants_a_whole_non_negative_number() {
        assert_eq!(Value::Num(7.0).as_u64(), Some(7));
        assert_eq!(Value::Num(7.5).as_u64(), None);
        assert_eq!(Value::Num(-1.0).as_u64(), None);
        assert_eq!(Value::from("7").as_u64(), None);
    }

    #[test]
    fn the_last_duplicate_key_wins() {
        assert_eq!(ok(r#"{"a":1,"a":2}"#).get("a"), Some(&Value::Num(2.0)));
    }

    #[test]
    fn empty_containers_and_whitespace() {
        assert_eq!(ok(" [ ] "), Value::Arr(vec![]));
        assert_eq!(ok("{ }"), Value::Obj(vec![]));
        assert_eq!(
            ok("[1 , 2\n,\t3]"),
            Value::Arr(vec![1u64.into(), 2u64.into(), 3u64.into()])
        );
    }

    #[test]
    fn malformed_input_is_refused() {
        for bad in [
            "",
            " ",
            "[1,]",
            "{\"a\":1,}",
            "[1 2]",
            "{\"a\" 1}",
            "{a:1}",
            "01",
            "1.",
            ".5",
            "+1",
            "-",
            "1e",
            "1e+",
            "nul",
            "tru",
            "\"abc",
            "\"a\nb\"",
            "\"\\x\"",
            "\"\\u12\"",
            "\"\\ud800\"",
            "\"\\ud800\\u0041\"",
            "\"\\udc00\"",
            "[1] x",
            "{\"a\":}",
            "[",
            "{",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn nesting_is_limited_instead_of_overflowing_the_stack() {
        let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
        assert!(parse(&deep(64)).is_ok());
        let err = parse(&deep(10_000)).unwrap_err();
        assert_eq!(err.what, "nested too deeply");
    }

    #[test]
    fn the_error_says_where() {
        let err = parse("[1, 2, x]").unwrap_err();
        assert_eq!(err.at, 7);
        assert!(err.to_string().contains("byte 7"));
    }

    #[test]
    fn writing_is_compact_and_escapes_what_it_must() {
        let v = Value::obj([
            ("s", "q\"b\\n\n\u{1}\u{e9}".into()),
            ("n", 3.0.into()),
            ("f", 0.5.into()),
            ("a", Value::Arr(vec![Value::Null, true.into()])),
        ]);
        assert_eq!(
            v.to_string(),
            r#"{"s":"q\"b\\n\n\u0001é","n":3,"f":0.5,"a":[null,true]}"#
        );
    }

    #[test]
    fn numbers_that_json_cannot_hold_become_null() {
        for n in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(Value::Num(n).to_string(), "null");
        }
    }

    #[test]
    fn large_and_small_numbers_survive_a_round_trip() {
        for n in [
            1e15,
            1.5e300,
            1e-7,
            -123_456_789.125,
            9_007_199_254_740_993.0,
        ] {
            let text = Value::Num(n).to_string();
            assert_eq!(ok(&text), Value::Num(n), "{text}");
        }
    }

    fn random_value(rng: &mut Rng, depth: u32) -> Value {
        match rng.below(if depth == 0 { 4 } else { 6 }) {
            0 => Value::Null,
            1 => Value::Bool(rng.below(2) == 0),
            2 => Value::Num(rng.below(2_000_000) as f64 / 8.0 - 100_000.0),
            3 => {
                let alphabet = [
                    'a',
                    'Z',
                    ' ',
                    '"',
                    '\\',
                    '\n',
                    '\t',
                    '\u{1}',
                    '\u{e9}',
                    '\u{1f600}',
                    '/',
                ];
                let len = rng.below(8);
                Value::Str(
                    (0..len)
                        .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
                        .collect(),
                )
            }
            4 => Value::Arr(
                (0..rng.below(4))
                    .map(|_| random_value(rng, depth - 1))
                    .collect(),
            ),
            _ => Value::Obj(
                (0..rng.below(4))
                    .map(|i| (format!("k{i}\"\u{e9}"), random_value(rng, depth - 1)))
                    .collect(),
            ),
        }
    }

    #[test]
    fn whatever_is_written_can_be_read_back() {
        let mut rng = Rng::new(99);
        for _ in 0..2_000 {
            let value = random_value(&mut rng, 4);
            let text = value.to_string();
            assert_eq!(ok(&text), value, "{text}");
        }
    }
}
