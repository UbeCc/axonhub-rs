//! Fast, allocation-light handling of the top-level `model` field.
//!
//! Request bodies can be megabytes (system prompts, tool catalogues, images)
//! and almost all of them are forwarded byte-for-byte.  Parsing the whole
//! document into `serde_json::Value` just to read or rewrite one string would
//! waste both CPU and allocations, so this module scans the top level and
//! stops at the `model` member.

use bytes::Bytes;

/// Read the top-level `model` string without building a JSON tree.
pub fn extract_model(body: &[u8]) -> Option<String> {
    let mut scanner = Scanner::new(body);
    scanner.skip_ws();
    scanner.expect(b'{')?;
    scanner.skip_ws();
    if scanner.peek() == Some(b'}') {
        return None;
    }
    loop {
        scanner.skip_ws();
        let key = scanner.parse_string()?;
        scanner.skip_ws();
        scanner.expect(b':')?;
        scanner.skip_ws();
        if key == b"model" {
            let value = scanner.parse_string()?;
            return Some(String::from_utf8_lossy(&value).into_owned());
        }
        scanner.skip_value()?;
        scanner.skip_ws();
        match scanner.peek()? {
            b',' => {
                scanner.pos += 1;
            }
            b'}' => return None,
            _ => return None,
        }
    }
}

/// Return `body` with its top-level `model` string replaced by `model`.
///
/// Every other byte is preserved exactly, which matters for upstream relays
/// that fingerprint the request body.  Returns the body unchanged when there
/// is no top-level `model` field.
pub fn replace_model(body: &[u8], model: &str) -> Bytes {
    let mut scanner = Scanner::new(body);
    scanner.skip_ws();
    if scanner.expect(b'{').is_none() {
        return Bytes::copy_from_slice(body);
    }
    loop {
        scanner.skip_ws();
        let key = match scanner.parse_string() {
            Some(key) => key,
            None => return Bytes::copy_from_slice(body),
        };
        scanner.skip_ws();
        if scanner.expect(b':').is_none() {
            return Bytes::copy_from_slice(body);
        }
        scanner.skip_ws();
        if key == b"model" && scanner.peek() == Some(b'"') {
            let start = scanner.pos;
            if scanner.parse_string().is_none() {
                return Bytes::copy_from_slice(body);
            }
            let end = scanner.pos;
            let mut out = Vec::with_capacity(body.len() + model.len());
            out.extend_from_slice(&body[..start]);
            out.push(b'"');
            out.extend_from_slice(model.as_bytes());
            out.push(b'"');
            out.extend_from_slice(&body[end..]);
            return Bytes::from(out);
        }
        if scanner.skip_value().is_none() {
            return Bytes::copy_from_slice(body);
        }
        scanner.skip_ws();
        match scanner.peek() {
            Some(b',') => scanner.pos += 1,
            _ => return Bytes::copy_from_slice(body),
        }
    }
}

struct Scanner<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        if self.peek()? == byte {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn skip_ws(&mut self) {
        while let Some(byte) = self.peek() {
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Parse a JSON string starting at `"`, returning the decoded bytes
    /// (escapes are decoded; `\uXXXX` is kept as its UTF-8 form).
    fn parse_string(&mut self) -> Option<Vec<u8>> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let byte = self.peek()?;
            self.pos += 1;
            match byte {
                b'"' => return Some(out),
                b'\\' => {
                    let escaped = self.peek()?;
                    self.pos += 1;
                    match escaped {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let code = self.parse_hex4()?;
                            push_utf8(&mut out, code)?;
                        }
                        _ => return None,
                    }
                }
                b if b < 0x20 => return None,
                b => out.push(b),
            }
        }
    }

    fn parse_hex4(&mut self) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.peek()?;
            self.pos += 1;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return None,
            };
            value = value * 16 + digit as u32;
        }
        Some(value)
    }

    fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => {
                self.parse_string()?;
                Some(())
            }
            b'{' | b'[' => {
                let mut stack = vec![self.peek()?];
                self.pos += 1;
                while let Some(top) = stack.last().copied() {
                    let byte = self.peek()?;
                    self.pos += 1;
                    match byte {
                        b'"' => {
                            // Rewind one byte and let parse_string consume it.
                            self.pos -= 1;
                            self.parse_string()?;
                        }
                        b'{' | b'[' => stack.push(byte),
                        b'}' => {
                            if top != b'{' {
                                return None;
                            }
                            stack.pop();
                        }
                        b']' => {
                            if top != b'[' {
                                return None;
                            }
                            stack.pop();
                        }
                        _ => {}
                    }
                }
                Some(())
            }
            _ => {
                // number / true / false / null
                while let Some(byte) = self.peek() {
                    if matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.pos += 1;
                }
                Some(())
            }
        }
    }
}

fn push_utf8(out: &mut Vec<u8>, code: u32) -> Option<()> {
    // Surrogates are kept as replacement characters; model names never use them.
    let ch = char::from_u32(code).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_model() {
        let body = br#"{"model":"claude-opus-5-5","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#;
        assert_eq!(extract_model(body).as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn extracts_model_with_escaped_strings_before_it() {
        let body =
            br#"{"system":"a \"quoted\" {brace}","model":"claude-sonnet-4-5","stream":true}"#;
        assert_eq!(extract_model(body).as_deref(), Some("claude-sonnet-4-5"));
    }

    #[test]
    fn replaces_model_without_touching_anything_else() {
        let body = br#"{"max_tokens":10, "model" : "old", "messages":[{"role":"user","content":"say \"OK\""}]}"#;
        let out = replace_model(body, "new");
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            r#"{"max_tokens":10, "model" : "new", "messages":[{"role":"user","content":"say \"OK\""}]}"#
        );
    }

    #[test]
    fn missing_model_is_unchanged() {
        let body = br#"{"messages":[]}"#;
        assert_eq!(&replace_model(body, "x")[..], &body[..]);
        assert_eq!(extract_model(body), None);
    }

    #[test]
    fn nested_model_fields_are_ignored() {
        let body = br#"{"metadata":{"model":"nested"},"model":"top"}"#;
        assert_eq!(extract_model(body).as_deref(), Some("top"));
        let out = replace_model(body, "new");
        assert!(std::str::from_utf8(&out)
            .unwrap()
            .contains(r#""model":"nested""#));
        assert!(std::str::from_utf8(&out)
            .unwrap()
            .contains(r#""model":"new""#));
    }
}
