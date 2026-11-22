use std::{
    collections::BTreeMap,
    fmt::{self, Write},
};

pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 32 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn strings(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|s| quote(s))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// A small JSON value used to preserve complete daemon reports when adding CLI
/// metadata. Numbers retain their original text, including integers above 2^53.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(fields) => fields.get(key),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Self> {
        match self {
            Self::Object(fields) => fields.get_mut(key),
            _ => None,
        }
    }

    pub fn set(&mut self, key: &str, value: Self) -> Result<(), String> {
        match self {
            Self::Object(fields) => {
                fields.insert(key.to_owned(), value);
                Ok(())
            }
            _ => Err("expected a JSON object".to_owned()),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) => value.parse().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(value) => Some(value),
            _ => None,
        }
    }
}

pub fn object<'a>(fields: impl IntoIterator<Item = (&'a str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}
impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}
impl From<u64> for Value {
    fn from(value: u64) -> Self {
        Self::Number(value.to_string())
    }
}
impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => output.write_str("null"),
            Self::Bool(value) => write!(output, "{value}"),
            Self::Number(value) => output.write_str(value),
            Self::String(value) => output.write_str(&quote(value)),
            Self::Array(values) => {
                output.write_str("[")?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.write_str(",")?;
                    }
                    write!(output, "{value}")?;
                }
                output.write_str("]")
            }
            Self::Object(fields) => {
                output.write_str("{")?;
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        output.write_str(",")?;
                    }
                    write!(output, "{}:{value}", quote(key))?;
                }
                output.write_str("}")
            }
        }
    }
}

pub fn parse(text: &str) -> Result<Value, String> {
    if text.len() > 1024 * 1024 {
        return Err("JSON report exceeds 1 MiB".to_owned());
    }
    let mut parser = Parser { text, offset: 0 };
    let value = parser.value(0)?;
    parser.whitespace();
    if parser.offset != text.len() {
        return Err(parser.error("trailing JSON input"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a str,
    offset: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> String {
        format!("{message} at byte {}", self.offset)
    }
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.offset).copied()
    }
    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.offset += 1;
        }
    }
    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > 64 {
            return Err(self.error("JSON nesting exceeds 64 levels"));
        }
        self.whitespace();
        match self.peek() {
            Some(b'"') => self.string().map(Value::String),
            Some(b'{') => {
                self.offset += 1;
                self.whitespace();
                let mut fields = BTreeMap::new();
                if self.consume(b'}') {
                    return Ok(Value::Object(fields));
                }
                loop {
                    self.whitespace();
                    let key = self.string()?;
                    self.whitespace();
                    if !self.consume(b':') {
                        return Err(self.error("expected ':'"));
                    }
                    let value = self.value(depth + 1)?;
                    if fields.insert(key, value).is_some() {
                        return Err(self.error("duplicate JSON object key"));
                    }
                    self.whitespace();
                    if self.consume(b'}') {
                        break;
                    }
                    if !self.consume(b',') {
                        return Err(self.error("expected ',' or '}'"));
                    }
                }
                Ok(Value::Object(fields))
            }
            Some(b'[') => {
                self.offset += 1;
                self.whitespace();
                let mut values = Vec::new();
                if self.consume(b']') {
                    return Ok(Value::Array(values));
                }
                loop {
                    values.push(self.value(depth + 1)?);
                    self.whitespace();
                    if self.consume(b']') {
                        break;
                    }
                    if !self.consume(b',') {
                        return Err(self.error("expected ',' or ']'"));
                    }
                }
                Ok(Value::Array(values))
            }
            Some(b'n') => {
                self.literal("null")?;
                Ok(Value::Null)
            }
            Some(b't') => {
                self.literal("true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Value::Bool(false))
            }
            Some(b'-' | b'0'..=b'9') => self.number().map(Value::Number),
            _ => Err(self.error("expected a JSON value")),
        }
    }
    fn literal(&mut self, literal: &str) -> Result<(), String> {
        if self.text[self.offset..].starts_with(literal) {
            self.offset += literal.len();
            Ok(())
        } else {
            Err(self.error("invalid JSON literal"))
        }
    }
    fn number(&mut self) -> Result<String, String> {
        let start = self.offset;
        self.consume(b'-');
        if !self.consume(b'0') {
            if !matches!(self.peek(), Some(b'1'..=b'9')) {
                return Err(self.error("invalid JSON number"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.offset += 1;
            }
        }
        if self.consume(b'.') {
            self.digits()?;
        }
        if self.consume(b'e') || self.consume(b'E') {
            if !self.consume(b'+') {
                self.consume(b'-');
            }
            self.digits()?;
        }
        Ok(self.text[start..self.offset].to_owned())
    }
    fn digits(&mut self) -> Result<(), String> {
        let start = self.offset;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.offset += 1;
        }
        if self.offset == start {
            Err(self.error("expected number digits"))
        } else {
            Ok(())
        }
    }
    fn hex_quad(&mut self) -> Result<u16, String> {
        let mut value = 0u16;
        for _ in 0..4 {
            let digit = match self.peek() {
                Some(b'0'..=b'9') => self.peek().unwrap() - b'0',
                Some(b'a'..=b'f') => self.peek().unwrap() - b'a' + 10,
                Some(b'A'..=b'F') => self.peek().unwrap() - b'A' + 10,
                _ => return Err(self.error("invalid Unicode escape")),
            };
            value = (value << 4) | u16::from(digit);
            self.offset += 1;
        }
        Ok(value)
    }
    fn string(&mut self) -> Result<String, String> {
        if !self.consume(b'"') {
            return Err(self.error("expected a JSON string"));
        }
        let mut output = String::new();
        let mut chunk = self.offset;
        loop {
            match self.peek() {
                Some(b'"') => {
                    output.push_str(&self.text[chunk..self.offset]);
                    self.offset += 1;
                    return Ok(output);
                }
                Some(b'\\') => {
                    output.push_str(&self.text[chunk..self.offset]);
                    self.offset += 1;
                    let escape = self
                        .peek()
                        .ok_or_else(|| self.error("incomplete string escape"))?;
                    self.offset += 1;
                    match escape {
                        b'"' => output.push('"'),
                        b'\\' => output.push('\\'),
                        b'/' => output.push('/'),
                        b'b' => output.push('\u{8}'),
                        b'f' => output.push('\u{c}'),
                        b'n' => output.push('\n'),
                        b'r' => output.push('\r'),
                        b't' => output.push('\t'),
                        b'u' => {
                            let high = self.hex_quad()?;
                            let scalar = if (0xd800..=0xdbff).contains(&high) {
                                if !self.consume(b'\\') || !self.consume(b'u') {
                                    return Err(self.error("missing Unicode low surrogate"));
                                }
                                let low = self.hex_quad()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return Err(self.error("invalid Unicode low surrogate"));
                                }
                                0x10000 + ((u32::from(high) - 0xd800) << 10) + u32::from(low)
                                    - 0xdc00
                            } else {
                                u32::from(high)
                            };
                            output.push(
                                char::from_u32(scalar)
                                    .ok_or_else(|| self.error("invalid Unicode scalar"))?,
                            );
                        }
                        _ => return Err(self.error("invalid string escape")),
                    }
                    chunk = self.offset;
                }
                Some(0..=31) => return Err(self.error("unescaped control character")),
                Some(_) => self.offset += 1,
                None => return Err(self.error("unterminated JSON string")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn untrusted_names_are_escaped() {
        assert_eq!(super::quote("a\"\\\n\u{1}中"), "\"a\\\"\\\\\\n\\u0001中\"");
    }

    #[test]
    fn reports_round_trip_nested_names_unicode_and_large_counters() {
        let text = r#"{"errors":["brace } and \"processes\": []"],"n":18446744073709551615,"future":{"float":-1.25e+3,"ok":true},"processes":[{"name":"\u4e2d\ud83d\ude00\n"}]}"#;
        let value = super::parse(text).unwrap();
        assert_eq!(
            value.get("n").and_then(super::Value::as_u64),
            Some(u64::MAX)
        );
        assert_eq!(super::parse(&value.to_string()).unwrap(), value);
        assert_eq!(
            value
                .get("processes")
                .and_then(super::Value::as_array)
                .unwrap()[0]
                .get("name")
                .and_then(super::Value::as_str),
            Some("中😀\n")
        );
    }

    #[test]
    fn ambiguous_truncated_or_overdeep_reports_are_rejected() {
        for text in [
            "{\"pid\":1,\"pid\":2}",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1.",
            "1e",
            "true false",
            "\"\\ud800\"",
            "\"\\udc00\"",
            "\"raw\nline\"",
            "{\"a\":",
        ] {
            assert!(super::parse(text).is_err(), "{text}");
        }
        assert!(super::parse(&format!("{}0{}", "[".repeat(66), "]".repeat(66))).is_err());
    }
}
