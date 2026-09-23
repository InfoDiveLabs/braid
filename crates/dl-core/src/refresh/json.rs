//! A JSON reader just large enough for refresher replies.
//!
//! Hand-written rather than `serde_json`: the engine parses two shapes, and
//! `dl` is size-gated, so a general deserializer would cost more than the
//! feature it serves.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    pub fn parse(input: &str) -> std::result::Result<Json, String> {
        let mut parser = Parser { bytes: input.as_bytes(), at: 0 };
        parser.skip_whitespace();
        let value = parser.value()?;
        parser.skip_whitespace();
        if parser.at != parser.bytes.len() {
            return Err(format!("trailing data at byte {}", parser.at));
        }
        Ok(value)
    }

    /// Look up a dotted path, `a.b.c`. Array indices are not addressable; a
    /// refresher that needs one should return a flatter shape.
    pub fn get(&self, path: &str) -> Option<&Json> {
        let mut node = self;
        for segment in path.split('.').filter(|s| !s.is_empty()) {
            node = match node {
                Json::Object(map) => map.get(segment)?,
                _ => return None,
            };
        }
        Some(node)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// String-valued members, in order. Used for header maps.
    pub fn as_string_pairs(&self) -> Option<Vec<(String, String)>> {
        match self {
            Json::Object(map) => Some(
                map.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect(),
            ),
            _ => None,
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn skip_whitespace(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn expect(&mut self, byte: u8) -> std::result::Result<(), String> {
        if self.peek() == Some(byte) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("expected {:?} at byte {}", byte as char, self.at))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> std::result::Result<Json, String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(value)
        } else {
            Err(format!("unrecognised literal at byte {}", self.at))
        }
    }

    fn value(&mut self) -> std::result::Result<Json, String> {
        match self.peek().ok_or_else(|| "unexpected end of input".to_string())? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Json::String(self.string()?)),
            b't' => self.literal("true", Json::Bool(true)),
            b'f' => self.literal("false", Json::Bool(false)),
            b'n' => self.literal("null", Json::Null),
            _ => self.number(),
        }
    }

    fn object(&mut self) -> std::result::Result<Json, String> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Json::Object(map));
        }
        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            map.insert(key, self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err(format!("unterminated object at byte {}", self.at)),
            }
        }
    }

    fn array(&mut self) -> std::result::Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(format!("unterminated array at byte {}", self.at)),
            }
        }
    }

    fn string(&mut self) -> std::result::Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let byte = self.peek().ok_or_else(|| "unterminated string".to_string())?;
            self.at += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = self.peek().ok_or_else(|| "truncated escape".to_string())?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        other => return Err(format!("unknown escape \\{}", other as char)),
                    }
                }
                _ => {
                    // Multi-byte UTF-8 arrives one byte at a time; collect the
                    // whole sequence rather than pushing invalid scalars.
                    let start = self.at - 1;
                    let width = utf8_width(byte);
                    self.at = start + width;
                    let slice = self
                        .bytes
                        .get(start..self.at)
                        .ok_or_else(|| "truncated utf-8".to_string())?;
                    out.push_str(std::str::from_utf8(slice).map_err(|e| e.to_string())?);
                }
            }
        }
    }

    /// Surrogate pairs are joined; a lone surrogate becomes the replacement
    /// character rather than failing the whole parse.
    fn unicode_escape(&mut self) -> std::result::Result<char, String> {
        let first = self.hex4()?;
        if (0xD800..0xDC00).contains(&first) {
            if self.bytes[self.at..].starts_with(b"\\u") {
                self.at += 2;
                let second = self.hex4()?;
                let combined =
                    0x1_0000 + ((first as u32 - 0xD800) << 10) + (second as u32 - 0xDC00);
                return char::from_u32(combined).ok_or_else(|| "invalid surrogate pair".into());
            }
            return Ok(char::REPLACEMENT_CHARACTER);
        }
        Ok(char::from_u32(first as u32).unwrap_or(char::REPLACEMENT_CHARACTER))
    }

    fn hex4(&mut self) -> std::result::Result<u16, String> {
        let slice = self.bytes.get(self.at..self.at + 4).ok_or("truncated \\u escape")?;
        let text = std::str::from_utf8(slice).map_err(|e| e.to_string())?;
        let value = u16::from_str_radix(text, 16).map_err(|e| e.to_string())?;
        self.at += 4;
        Ok(value)
    }

    fn number(&mut self) -> std::result::Result<Json, String> {
        let start = self.at;
        while matches!(self.peek(), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
            self.at += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).map_err(|e| e.to_string())?;
        text.parse().map(Json::Number).map_err(|_| format!("invalid number {text:?}"))
    }
}

fn utf8_width(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refresher_reply_is_read_back_whole() {
        let value = Json::parse(
            r#"{"url": "https://cdn.test/a.bin?sig=1", "headers": {"Cookie": "s=1"}, "expires_in": 900}"#,
        )
        .expect("a well-formed reply should parse");

        assert_eq!(value.get("url").and_then(Json::as_str), Some("https://cdn.test/a.bin?sig=1"));
        assert_eq!(
            value.get("headers").and_then(Json::as_string_pairs),
            Some(vec![("Cookie".to_string(), "s=1".to_string())])
        );
        assert_eq!(value.get("expires_in").and_then(Json::as_f64), Some(900.0));
    }

    #[test]
    fn a_nested_path_addresses_the_url_an_api_buries() {
        let value =
            Json::parse(r#"{"data": {"stream": {"href": "https://x.test/v.mp4"}}}"#).unwrap();
        assert_eq!(
            value.get("data.stream.href").and_then(Json::as_str),
            Some("https://x.test/v.mp4")
        );
        assert_eq!(value.get("data.missing.href"), None);
    }

    #[test]
    fn escapes_and_non_ascii_survive_the_round_trip() {
        let value = Json::parse(r#"{"a": "line\nbreak \"q\" é café é 😀"}"#).unwrap();
        assert_eq!(value.get("a").and_then(Json::as_str), Some("line\nbreak \"q\" é café é 😀"));
    }

    #[test]
    fn malformed_input_is_an_error_rather_than_a_wrong_url() {
        // A refresher that half-parses garbage would hand the downloader a
        // plausible-looking URL built from nothing.
        for input in ["{", "{\"a\":}", "{\"a\": 1} trailing", "", "{\"a\" 1}"] {
            assert!(Json::parse(input).is_err(), "{input:?} should not parse");
        }
    }
}
