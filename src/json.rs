use std::collections::BTreeMap;
use std::fmt::{self, Write};

use thiserror::Error;

const MAX_DEPTH: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid JSON at byte {position}: {reason}")]
pub struct ParseError {
    position: usize,
    reason: &'static str,
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, ParseError> {
        let mut parser = Parser {
            bytes: text.as_bytes(),
            position: 0,
        };
        let value = parser.value(0)?;
        parser.skip_whitespace();
        if parser.position != parser.bytes.len() {
            return Err(parser.error("trailing characters"));
        }
        Ok(value)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(map) => map.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn object<const N: usize>(entries: [(&str, Json); N]) -> Json {
        Json::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Json::String(value.to_string())
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Json::Bool(value)
    }
}

impl From<u64> for Json {
    fn from(value: u64) -> Self {
        Json::Number(value as f64)
    }
}

fn write_string(out: &mut fmt::Formatter, text: &str) -> fmt::Result {
    out.write_char('"')?;
    for c in text.chars() {
        match c {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            c if u32::from(c) < 0x20 => write!(out, "\\u{:04x}", u32::from(c))?,
            c => out.write_char(c)?,
        }
    }
    out.write_char('"')
}

impl fmt::Display for Json {
    fn fmt(&self, out: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Json::Null => out.write_str("null"),
            Json::Bool(b) => write!(out, "{b}"),
            Json::Number(n) if n.is_finite() && n.fract() == 0.0 && n.abs() < 9e15 => {
                write!(out, "{}", *n as i64)
            }
            Json::Number(n) if n.is_finite() => write!(out, "{n}"),
            Json::Number(_) => out.write_str("null"),
            Json::String(s) => write_string(out, s),
            Json::Array(items) => {
                out.write_char('[')?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.write_char(',')?;
                    }
                    write!(out, "{item}")?;
                }
                out.write_char(']')
            }
            Json::Object(map) => {
                out.write_char('{')?;
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.write_char(',')?;
                    }
                    write_string(out, key)?;
                    write!(out, ":{value}")?;
                }
                out.write_char('}')
            }
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn error(&self, reason: &'static str) -> ParseError {
        ParseError {
            position: self.position,
            reason,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), ParseError> {
        if self.peek() == Some(byte) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error("unexpected character"))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, ParseError> {
        if self.bytes[self.position..].starts_with(word.as_bytes()) {
            self.position += word.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::String),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.error("expected a value")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Ok(Json::Object(map));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b'}') => {
                    self.position += 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.position += 1,
                Some(b']') => {
                    self.position += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .bytes
            .get(self.position..self.position + 4)
            .ok_or_else(|| self.error("truncated escape"))?;
        let text = std::str::from_utf8(digits).map_err(|_| self.error("invalid escape"))?;
        let value = u32::from_str_radix(text, 16).map_err(|_| self.error("invalid escape"))?;
        self.position += 4;
        Ok(value)
    }

    fn escape(&mut self) -> Result<char, ParseError> {
        let byte = self.peek().ok_or_else(|| self.error("truncated escape"))?;
        self.position += 1;
        Ok(match byte {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let high = self.hex4()?;
                let code = if (0xd800..0xdc00).contains(&high) {
                    if !self.bytes[self.position..].starts_with(b"\\u") {
                        return Err(self.error("unpaired surrogate"));
                    }
                    self.position += 2;
                    let low = self.hex4()?;
                    if !(0xdc00..0xe000).contains(&low) {
                        return Err(self.error("unpaired surrogate"));
                    }
                    0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                } else {
                    high
                };
                char::from_u32(code).ok_or_else(|| self.error("invalid code point"))?
            }
            _ => return Err(self.error("invalid escape")),
        })
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let start = self.position;
            while matches!(self.peek(), Some(b) if b != b'"' && b != b'\\' && b >= 0x20) {
                self.position += 1;
            }
            let chunk = std::str::from_utf8(&self.bytes[start..self.position])
                .map_err(|_| self.error("invalid UTF-8"))?;
            out.push_str(chunk);
            match self.peek() {
                Some(b'"') => {
                    self.position += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.position += 1;
                    out.push(self.escape()?);
                }
                Some(_) => return Err(self.error("control character in string")),
                None => return Err(self.error("unterminated string")),
            }
        }
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.position;
        while matches!(
            self.peek(),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.position += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.position])
            .ok()
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|n| n.is_finite())
            .map(Json::Number)
            .ok_or_else(|| self.error("invalid number"))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parses_commands() {
        let value =
            Json::parse(r#" {"cmd": "snapshot", "path": "/tmp/a b.snap", "n": 3} "#).unwrap();
        assert_eq!(value.get("cmd").and_then(Json::as_str), Some("snapshot"));
        assert_eq!(
            value.get("path").and_then(Json::as_str),
            Some("/tmp/a b.snap")
        );
        assert_eq!(value.get("n"), Some(&Json::Number(3.0)));
        assert_eq!(value.get("missing"), None);
    }

    #[test]
    fn parses_nested_values_and_escapes() {
        let value =
            Json::parse(r#"[true, false, null, -1.5e2, "a\"\\\n\u00e9\ud83d\ude00", {}, []]"#)
                .unwrap();
        let Json::Array(items) = value else {
            panic!("expected array");
        };
        assert_eq!(items[3], Json::Number(-150.0));
        assert_eq!(items[4], Json::String("a\"\\\né😀".into()));
    }

    #[test]
    fn rejects_invalid_documents() {
        for text in [
            "",
            "{",
            "{\"a\"}",
            "[1,]",
            "tru",
            "\"abc",
            "{\"a\":1}x",
            "\"\\x\"",
            "\"\\ud800\"",
            "1e999",
            "\"\u{1}\"",
        ] {
            assert!(Json::parse(text).is_err(), "{text:?} should be rejected");
        }
        let deep = "[".repeat(100) + &"]".repeat(100);
        assert!(Json::parse(&deep).is_err());
    }

    #[test]
    fn serializes_objects() {
        let value = Json::object([
            ("ok", Json::from(true)),
            ("state", Json::from("running")),
            ("vcpus", Json::from(2u64)),
            ("msg", Json::from("line\n\"q\"")),
        ]);
        assert_eq!(
            value.to_string(),
            r#"{"msg":"line\n\"q\"","ok":true,"state":"running","vcpus":2}"#
        );
        assert_eq!(Json::Number(1.5).to_string(), "1.5");
    }

    proptest! {
        #[test]
        fn strings_round_trip(text in ".*") {
            let encoded = Json::String(text.clone()).to_string();
            prop_assert_eq!(Json::parse(&encoded).unwrap(), Json::String(text));
        }

        #[test]
        fn arbitrary_input_never_panics(text in "\\PC*") {
            let _ = Json::parse(&text);
        }
    }
}
