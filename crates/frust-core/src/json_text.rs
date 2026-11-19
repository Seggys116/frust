//! JSON string decoding and encoding shared by config parsing and `fcargo` diagnostics.

#[allow(dead_code)] // fcargo calls this; the Cargo library uses decode_json_string_body.
pub fn unescape_json_string(input: &str) -> Option<String> {
    decode_json_string_body(input.as_bytes()).ok().map(|(text, _)| text)
}

/// Escape text for a JSON string literal, without surrounding quotes. No raw control
/// character survives, and `unescape_json_string` inverts it once a closing quote is added.
#[allow(dead_code)] // emitted JSON in frust-core uses this; the Cargo library does not.
pub fn escape_json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out
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

/// Lone surrogates are rejected rather than replaced: a config path must not silently change.
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

/// Exactly four ASCII hex digits; `from_str_radix` would also accept a leading `+`.
fn hex4(bytes: &[u8], index: &mut usize) -> Result<u32, &'static str> {
    let hex = bytes.get(*index..*index + 4).ok_or("short unicode escape")?;
    *index += 4;
    let mut unit = 0u32;
    for &digit in hex {
        let value = (digit as char).to_digit(16).ok_or("bad unicode escape")?;
        unit = (unit << 4) | value;
    }
    Ok(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed-seed xorshift64 so generated cases are identical on every run.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next_u64() % bound as u64) as usize
        }
    }

    const INTERESTING: [char; 16] = [
        '"', '\\', '/', '\u{0}', '\u{8}', '\u{c}', '\n', '\r', '\t', '\u{1f}', '\u{7f}', 'a',
        '\u{e9}', '\u{2028}', '\u{1F600}', '\u{10FFFF}',
    ];

    fn random_text(rng: &mut Rng) -> String {
        let len = rng.below(24);
        (0..len)
            .map(|_| match rng.below(4) {
                0 => char::from_u32(rng.below(0x20) as u32).unwrap(),
                1 => char::from_u32(0x20 + rng.below(0x5f) as u32).unwrap(),
                _ => INTERESTING[rng.below(INTERESTING.len())],
            })
            .collect()
    }

    fn decode(body: &str) -> Option<String> {
        unescape_json_string(&format!("{body}\""))
    }

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

    #[test]
    fn decodes_every_single_character_escape() {
        let cases = [
            (r#"\""#, "\""),
            (r"\\", "\\"),
            (r"\/", "/"),
            (r"\b", "\u{8}"),
            (r"\f", "\u{c}"),
            (r"\n", "\n"),
            (r"\r", "\r"),
            (r"\t", "\t"),
        ];
        for (body, expected) in cases {
            assert_eq!(decode(body).as_deref(), Some(expected), "{body}");
        }
        for bad in [r"\a", r"\x41", r"\0", r"\U0041", r"\ "] {
            assert!(decode(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn unicode_escapes_need_exactly_four_hex_digits() {
        assert_eq!(decode(r"\u00e9").as_deref(), Some("\u{e9}"));
        assert_eq!(decode(r"\u00E9").as_deref(), Some("\u{e9}"));
        assert_eq!(decode(r"\u0000").as_deref(), Some("\u{0}"));
        for bad in [r"\u+04F", r"\u-001", r"\u 041", r"\u00g1", r"\u004", r"\u00\u0041", r"\u"] {
            assert!(decode(bad).is_none(), "{bad}");
        }
        // Non-ASCII bytes inside the four-byte window must not be read as digits.
        assert!(decode("\\u00\u{e9}").is_none());
    }

    #[test]
    fn surrogates_must_pair_in_order() {
        assert_eq!(decode(r"\uD83D\uDE00").as_deref(), Some("\u{1F600}"));
        assert_eq!(decode(r"\ud800\udc00").as_deref(), Some("\u{10000}"));
        assert_eq!(decode(r"\uDBFF\uDFFF").as_deref(), Some("\u{10FFFF}"));
        for bad in [
            r"\uD83D",
            r"\uD83Dx",
            r"\uD83D\n",
            r"\uD83D\u0041",
            r"\uD83D\uD83D",
            r"\uDE00\uD83D",
            r"\uDE00",
        ] {
            assert!(decode(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn raw_control_characters_and_bad_utf8_are_rejected() {
        for control in 0u8..0x20 {
            let mut body = b"ab".to_vec();
            body.push(control);
            body.extend_from_slice(b"\"");
            assert_eq!(decode_json_string_body(&body), Err("raw control character in string"));
        }
        assert_eq!(decode_json_string_body(b"\xff\""), Err("string is not UTF-8"));
        assert_eq!(decode_json_string_body(b"ab\\"), Err("unterminated escape"));
    }

    #[test]
    fn decode_reports_consumed_length_through_closing_quote() {
        let (text, consumed) = decode_json_string_body(br#"ab\n"tail"#).unwrap();
        assert_eq!(text, "ab\n");
        assert_eq!(consumed, 5);
    }

    #[test]
    fn escape_covers_every_control_character() {
        for code in 0u32..0x20 {
            let ch = char::from_u32(code).unwrap();
            let escaped = escape_json_string(&ch.to_string());
            assert!(escaped.starts_with('\\'), "{code:#x} -> {escaped}");
            assert!(escaped.bytes().all(|byte| (0x20..0x7f).contains(&byte)), "{escaped}");
            assert_eq!(decode(&escaped).unwrap(), ch.to_string());
        }
        assert_eq!(escape_json_string("\u{1}\u{1f}"), "\\u0001\\u001f");
        assert_eq!(escape_json_string("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(escape_json_string("\u{1F600}\u{e9}"), "\u{1F600}\u{e9}");
    }

    #[test]
    fn escape_then_unescape_round_trips_generated_text() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..2000 {
            let text = random_text(&mut rng);
            let escaped = escape_json_string(&text);
            assert!(escaped.bytes().all(|byte| byte >= 0x20), "{escaped:?}");
            let mut backslashes = 0;
            for byte in escaped.bytes() {
                assert!(byte != b'"' || backslashes % 2 == 1, "{escaped:?}");
                backslashes = if byte == b'\\' { backslashes + 1 } else { 0 };
            }
            let (decoded, consumed) =
                decode_json_string_body(format!("{escaped}\"").as_bytes()).unwrap();
            assert_eq!(decoded, text);
            assert_eq!(consumed, escaped.len() + 1);
        }
    }

    #[test]
    fn decoder_never_panics_on_random_bytes() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let alphabet = b"\\\"u0123456789abcdefABCDEF+-/ntbfrx\x00\x1f\x7f\x80\xc3\xa9\xf0\x9f\x98\x80";
        for _ in 0..5000 {
            let len = rng.below(40);
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    if rng.below(8) == 0 {
                        rng.next_u64() as u8
                    } else {
                        alphabet[rng.below(alphabet.len())]
                    }
                })
                .collect();
            if let Ok((_, consumed)) = decode_json_string_body(&bytes) {
                assert!(consumed >= 1 && consumed <= bytes.len());
                assert_eq!(bytes[consumed - 1], b'"');
            }
        }
    }
}
