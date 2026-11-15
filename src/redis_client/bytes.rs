//! Key names, collection members and channel names that aren't UTF-8.
//!
//! Redis names and members are byte strings; the app keeps them as `String`
//! everywhere (sweep, sort, tree, widgets). So that a msgpack member or a
//! key named `\xff\xfe` neither fails the whole page nor loses its identity,
//! a byte that isn't part of valid UTF-8 is kept as one private-use
//! character (`U+F780..=U+F7FF` for `0x80..=0xFF`; a byte below `0x80` is
//! always valid). [`Text`] decodes replies that way, [`Raw`] turns the text
//! back into the exact bytes for a command, and `escape::printable` shows
//! such a character as `\xHH`.
//!
//! The mapping is exact both ways: a real `U+F780..=U+F7FF` character in a
//! name is stored as its three UTF-8 bytes, each as a byte character.

use std::{borrow::Cow, fmt::Write as _};

use redis::{FromRedisValue, ParsingError, RedisWrite, ToRedisArgs, ToSingleRedisArg, Value};

const BYTE_CHAR_FIRST: u32 = 0xF780;
const BYTE_CHAR_LAST: u32 = 0xF7FF;
const BYTE_CHAR_BASE: u32 = 0xF700;

/// The raw byte `c` stands for, if it is a byte character.
#[must_use]
pub fn byte_of(c: char) -> Option<u8> {
    let code = u32::from(c);
    if (BYTE_CHAR_FIRST..=BYTE_CHAR_LAST).contains(&code) {
        u8::try_from(code - BYTE_CHAR_BASE).ok()
    } else {
        None
    }
}

fn byte_char(byte: u8) -> char {
    char::from_u32(BYTE_CHAR_BASE + u32::from(byte)).unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// Whether `text` holds bytes that aren't UTF-8 (see the module docs).
#[must_use]
pub fn is_binary(text: &str) -> bool {
    text.chars().any(|c| byte_of(c).is_some())
}

/// `bytes` as text, every byte outside valid UTF-8 as a byte character.
#[must_use]
pub fn text_from_bytes(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        if !is_binary(text) {
            return text.to_owned();
        }
    }
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if byte_of(c).is_some() {
                let mut buf = [0; 4];
                out.extend(c.encode_utf8(&mut buf).bytes().map(byte_char));
            } else {
                out.push(c);
            }
        }
        out.extend(chunk.invalid().iter().copied().map(byte_char));
    }
    out
}

/// The bytes `text` stands for: the reverse of [`text_from_bytes`].
#[must_use]
pub fn bytes_from_text(text: &str) -> Cow<'_, [u8]> {
    if !is_binary(text) {
        return Cow::Borrowed(text.as_bytes());
    }
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        if let Some(byte) = byte_of(c) {
            out.push(byte);
        } else {
            let mut buf = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    Cow::Owned(out)
}

/// `text` for a toast, an error or the clipboard: bytes that aren't UTF-8
/// as `\xHH`, everything else (line breaks included) untouched.
#[must_use]
pub fn shown(text: &str) -> Cow<'_, str> {
    if !is_binary(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match byte_of(c) {
            Some(byte) => {
                let _ = write!(out, "\\x{byte:02x}");
            }
            None => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// A key name, member, field or pattern as a command argument: its exact
/// bytes. Use it wherever such text goes into a command.
#[derive(Clone, Copy, Debug)]
pub struct Raw<'a>(pub &'a str);

impl ToRedisArgs for Raw<'_> {
    fn write_redis_args<W>(&self, out: &mut W)
    where
        W: ?Sized + RedisWrite,
    {
        out.write_arg(&bytes_from_text(self.0));
    }
}

impl ToSingleRedisArg for Raw<'_> {}

/// A name or member from a reply; never fails on bytes that aren't UTF-8.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Text(pub String);

impl FromRedisValue for Text {
    fn from_redis_value(v: Value) -> Result<Self, ParsingError> {
        let bytes = Vec::<u8>::from_redis_value(v)?;
        Ok(Self(text_from_bytes(&bytes)))
    }
}

/// Unwrap a list of [`Text`].
#[must_use]
pub fn strings(texts: Vec<Text>) -> Vec<String> {
    texts.into_iter().map(|text| text.0).collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::{bytes_from_text, is_binary, text_from_bytes};

    /// #65: any byte string survives the trip into text and back.
    #[test]
    fn bytes_round_trip_through_text() {
        let cases: &[&[u8]] = &[
            b"user:1",
            "café ☕ キー".as_bytes(),
            b"\xff\xfe",
            b"session:\x93\xa3abc\xc0",
            b"\xe2\x82",           // a cut-off '€'
            "\u{f7ff}".as_bytes(), // a real byte character
            b"a\xef\x9e\x80b\x80",
            b"",
        ];
        for raw in cases {
            let text = text_from_bytes(raw);
            assert_eq!(bytes_from_text(&text).as_ref(), *raw, "{raw:?}");
        }
        assert_eq!(text_from_bytes(b"user:1"), "user:1");
        assert!(!is_binary("café ☕"));
        assert!(is_binary(&text_from_bytes(b"\xff")));
        // Plain text costs no copy.
        assert!(matches!(
            bytes_from_text("user:1"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(super::shown(&text_from_bytes(b"k\xff\nv")), "k\\xff\nv");
        // Different byte strings never share a text.
        assert_ne!(text_from_bytes(b"\xff"), text_from_bytes(b"\xfe"));
        assert_ne!(
            text_from_bytes("\u{f7ff}".as_bytes()),
            text_from_bytes(b"\xff")
        );
    }
}
