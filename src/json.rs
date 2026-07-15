//! Minimal JSON parser for the Docker API payloads this app reads.
//!
//! Replaces serde: we only ever *read* JSON (never serialize), and only a
//! handful of fields per payload, so a small `Value` tree plus accessor
//! helpers is all that is needed.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Integral numbers keep full i64 precision (cpu counters are u64
    /// nanoseconds and can exceed f64's 2^53 mantissa).
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(HashMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(m) => m.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Int(i) => u64::try_from(*i).ok(),
            Value::Float(f) if *f >= 0.0 => Some(*f as u64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Arr(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&HashMap<String, Value>> {
        match self {
            Value::Obj(m) => Some(m),
            _ => None,
        }
    }

    /// `get(key)` then string copy, `None` if absent or not a string.
    pub fn str_of(&self, key: &str) -> Option<String> {
        self.get(key)?.as_str().map(str::to_string)
    }
}

pub fn parse(input: &str) -> Result<Value, String> {
    let mut p = Parser { b: input.as_bytes(), i: 0, depth: 0 };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err(format!("trailing data at byte {}", p.i));
    }
    Ok(v)
}

const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while let Some(c) = self.b.get(self.i) {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.i += 1,
                _ => break,
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.i))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("nesting too deep".into());
        }
        let v = match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => self.string().map(Value::Str),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(format!("unexpected byte at {}", self.i)),
        };
        self.depth -= 1;
        v
    }

    fn literal(&mut self, lit: &str, v: Value) -> Result<Value, String> {
        if self.b[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(v)
        } else {
            Err(format!("bad literal at byte {}", self.i))
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.eat(b'{')?;
        let mut m = HashMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Value::Obj(m));
        }
        loop {
            self.skip_ws();
            let k = self.string()?;
            self.skip_ws();
            self.eat(b':')?;
            self.skip_ws();
            let v = self.value()?;
            m.insert(k, v);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Obj(m));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.i)),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.eat(b'[')?;
        let mut v = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Value::Arr(v));
        }
        loop {
            self.skip_ws();
            v.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Arr(v));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.i)),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err("unterminated string".into()),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(s);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let esc = self.peek().ok_or("unterminated escape")?;
                    self.i += 1;
                    match esc {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'b' => s.push('\u{8}'),
                        b'f' => s.push('\u{c}'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                // surrogate pair
                                if self.peek() == Some(b'\\') {
                                    self.i += 1;
                                    self.eat(b'u')?;
                                    let lo = self.hex4()?;
                                    0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    return Err("lone surrogate".into());
                                }
                            } else {
                                hi
                            };
                            s.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(format!("bad escape at byte {}", self.i)),
                    }
                }
                Some(_) => {
                    // consume one utf-8 code point untouched
                    let start = self.i;
                    self.i += 1;
                    while self.i < self.b.len() && self.b[self.i] & 0xC0 == 0x80 {
                        self.i += 1;
                    }
                    s.push_str(
                        std::str::from_utf8(&self.b[start..self.i])
                            .map_err(|_| "invalid utf-8")?,
                    );
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let end = self.i + 4;
        if end > self.b.len() {
            return Err("short \\u escape".into());
        }
        let s = std::str::from_utf8(&self.b[self.i..end]).map_err(|_| "bad \\u escape")?;
        let v = u32::from_str_radix(s, 16).map_err(|_| "bad \\u escape")?;
        self.i = end;
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let mut float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => self.i += 1,
                b'.' | b'e' | b'E' | b'+' | b'-' => {
                    float = true;
                    self.i += 1;
                }
                _ => break,
            }
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).unwrap();
        if !float {
            if let Ok(i) = s.parse::<i64>() {
                return Ok(Value::Int(i));
            }
        }
        s.parse::<f64>()
            .map(Value::Float)
            .map_err(|_| format!("bad number at byte {start}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars() {
        assert_eq!(parse("null").unwrap(), Value::Null);
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("42").unwrap(), Value::Int(42));
        assert_eq!(parse("-7").unwrap(), Value::Int(-7));
        assert_eq!(parse("1.5").unwrap(), Value::Float(1.5));
        assert_eq!(parse("1e3").unwrap(), Value::Float(1000.0));
        assert_eq!(parse("\"hi\"").unwrap(), Value::Str("hi".into()));
    }

    #[test]
    fn big_integers_keep_precision() {
        // cpu counters are nanoseconds; > 2^53 must not lose bits
        let v = parse("9007199254740995").unwrap();
        assert_eq!(v.as_i64(), Some(9007199254740995));
        assert_eq!(v.as_u64(), Some(9007199254740995));
    }

    #[test]
    fn string_escapes() {
        assert_eq!(
            parse(r#""a\n\t\"\\Aé""#).unwrap(),
            Value::Str("a\n\t\"\\Aé".into())
        );
        // surrogate pair
        assert_eq!(parse(r#""😀""#).unwrap(), Value::Str("😀".into()));
    }

    #[test]
    fn nested_structures() {
        let v = parse(r#"{"a": [1, {"b": "x"}], "c": null, "d": {} }"#).unwrap();
        assert_eq!(v.get("a").unwrap().as_array().unwrap()[0].as_i64(), Some(1));
        assert_eq!(
            v.get("a").unwrap().as_array().unwrap()[1].str_of("b").as_deref(),
            Some("x")
        );
        assert_eq!(v.get("c"), Some(&Value::Null));
        assert!(v.get("d").unwrap().as_object().unwrap().is_empty());
    }

    #[test]
    fn unicode_passthrough() {
        assert_eq!(parse("\"zażółć 😀\"").unwrap(), Value::Str("zażółć 😀".into()));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_err());
        assert!(parse("{").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("{\"a\" 1}").is_err());
        assert!(parse("1 2").is_err());
        assert!(parse("\"abc").is_err());
    }

    #[test]
    fn deep_nesting_capped() {
        let s = "[".repeat(200) + &"]".repeat(200);
        assert!(parse(&s).is_err());
    }
}
