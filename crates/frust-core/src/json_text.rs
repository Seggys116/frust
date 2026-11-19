//! JSON string decoding shared by config parsing and `fcargo` diagnostics.

pub fn unescape_json_string(input: &str) -> Option<String> {
    decode_json_string_body(input.as_bytes()).ok().map(|(text, _)| text)
}

/// Decode a JSON string body that has already passed its opening quote.
/// The returned length includes the closing quote.
pub fn decode_json_string_body(bytes: &[u8]) -> Result<(String, usize), &'static str> {
    let mut out = Vec::new();
    let mut index = 0;
    loop {
        let byte = *bytes.get(index).ok_or("unterminated string")?;
        index += 1;
        match byte {
            b'"' => {
                let text = String::from_utf8(out).map_err(|_| "string is not UTF-8")?;
                return Ok((text, index));
            }
            b'\\' => {
                let escaped = *bytes.get(index).ok_or("unterminated escape")?;
                index += 1;
                match escaped {
                    b'"' => out.push(b'"'),
                    b'\\' => out.push(b'\\'),
                    b'/' => out.push(b'/'),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'u' => {
                        let ch = utf16_char(bytes, &mut index)?;
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                    }
                    _ => return Err("unsupported string escape"),
                }
            }
            byte if byte < 0x20 => return Err("raw control character in string"),
            byte => out.push(byte),
        }
    }
}

fn utf16_char(bytes: &[u8], index: &mut usize) -> Result<char, &'static str> {
    let unit = hex4(bytes, index)?;
    if (0xD800..=0xDBFF).contains(&unit) {
        if bytes.get(*index) != Some(&b'\\') || bytes.get(*index + 1) != Some(&b'u') {
            return Err("bad unicode escape");
        }
        *index += 2;
        let low = hex4(bytes, index)?;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return Err("bad unicode escape");
        }
        let code = 0x10000 + (((unit - 0xD800) << 10) | (low - 0xDC00));
        return char::from_u32(code).ok_or("bad unicode escape");
    }
    if (0xDC00..=0xDFFF).contains(&unit) {
        return Err("bad unicode escape");
    }
    char::from_u32(unit).ok_or("bad unicode escape")
}

fn hex4(bytes: &[u8], index: &mut usize) -> Result<u32, &'static str> {
    let hex = bytes.get(*index..*index + 4).ok_or("short unicode escape")?;
    *index += 4;
    let text = std::str::from_utf8(hex).map_err(|_| "bad unicode escape")?;
    u32::from_str_radix(text, 16).map_err(|_| "bad unicode escape")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_escapes_and_rejects_broken_unicode() {
        let text = unescape_json_string("line\\n\\t\\\"end\\\"\"").unwrap();
        assert_eq!(text, "line\n\t\"end\"");
        let snowman = unescape_json_string(r#"\u2603""#).unwrap();
        assert_eq!(snowman, "\u{2603}");
        let grin = unescape_json_string(r#"\uD83D\uDE00""#).unwrap();
        assert_eq!(grin, "\u{1F600}");
        let ansi = unescape_json_string(r#"\u001berror""#).unwrap();
        assert_eq!(ansi, "\u{1b}error");
        assert!(unescape_json_string(r#"\u12""#).is_none());
        assert!(unescape_json_string(r#"\uD800""#).is_none());
        assert!(unescape_json_string(r#"\uDEAD""#).is_none());
        assert!(unescape_json_string("unterminated").is_none());
    }
}
