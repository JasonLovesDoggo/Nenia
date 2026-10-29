//! Reversible lexical factoring, independent of any provider's JSON schema.
//!
//! Large canonical JSON strings are unescaped before sharing. Canonical base64
//! runs are decoded before sharing. Everything else is retained literally. A
//! small recipe addresses slices of authenticated chunks and reverses escapes.
use crate::pack::{Chunker, MAX_CHUNK_SIZE};
use anyhow::{ensure, Result};
use base64::{engine::general_purpose as b64, Engine};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};

const MIN_VALUE: usize = 256;
const MAX_LINE: usize = 32 * 1024 * 1024;
const MAX_DEPTH: u8 = 3;
pub const MAX_OPERATIONS: usize = 2_000_000;
const BASE64: u8 = 8;
const URL: u8 = 16;
const NO_PAD: u8 = 32;

/// [chunk table index, slice offset, slice length, reversible transform flags].
pub type Operation = [u32; 4];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub operations: Vec<Operation>,
    pub decoded_bytes: u64,
    pub decoded_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

struct Builder<F> {
    store: F,
    small: bool,
    operations: Vec<Operation>,
    literals: Vec<u8>,
    literal_ops: Vec<usize>,
}

impl<F: FnMut(&[u8]) -> Result<u32>> Builder<F> {
    fn op(&mut self, op: Operation) -> Result<()> {
        ensure!(
            self.operations.len() < MAX_OPERATIONS,
            "structural recipe exceeds operation limit"
        );
        self.operations.push(op);
        Ok(())
    }

    fn flush_literals(&mut self) -> Result<()> {
        if !self.literals.is_empty() {
            let index = (self.store)(&self.literals)?;
            for op in self.literal_ops.drain(..) {
                self.operations[op][0] = index;
            }
            self.literals.clear();
        }
        Ok(())
    }

    fn literal(&mut self, mut bytes: &[u8], flags: u8) -> Result<()> {
        while !bytes.is_empty() {
            if self.literals.len() == MAX_CHUNK_SIZE {
                self.flush_literals()?;
            }
            let len = bytes.len().min(MAX_CHUNK_SIZE - self.literals.len());
            let offset = self.literals.len() as u32;
            if let Some(last) = self.operations.last_mut().filter(|last| {
                last[0] == u32::MAX && last[1] + last[2] == offset && last[3] == flags as u32
            }) {
                last[2] += len as u32;
            } else {
                self.literal_ops.push(self.operations.len());
                self.op([u32::MAX, offset, len as u32, flags as u32])?;
            }
            self.literals.extend_from_slice(&bytes[..len]);
            bytes = &bytes[len..];
        }
        Ok(())
    }

    fn atom(&mut self, bytes: &[u8], flags: u8) -> Result<()> {
        if flags & BASE64 != 0 && !self.small {
            for part in bytes.chunks(MAX_CHUNK_SIZE / 3 * 3) {
                let index = (self.store)(part)?;
                self.op([index, 0, part.len() as u32, flags as u32])?;
            }
        } else if flags & BASE64 != 0 {
            // CDC boundaries aligned to three bytes preserve independent base64
            // expansion while finding overlap inside different image containers.
            let mut boundary = 0;
            let mut start = 0;
            for part in Chunker::new_small(bytes) {
                boundary += part?.len();
                let end = if boundary == bytes.len() {
                    boundary
                } else {
                    boundary / 3 * 3
                };
                if end > start {
                    let part = &bytes[start..end];
                    let index = (self.store)(part)?;
                    self.op([index, 0, part.len() as u32, flags as u32])?;
                    start = end;
                }
            }
        } else {
            let chunker = if self.small {
                Chunker::new_small(bytes)
            } else {
                Chunker::new(bytes)
            };
            for part in chunker {
                let part = part?;
                let index = (self.store)(&part)?;
                self.op([index, 0, part.len() as u32, flags as u32])?;
            }
        }
        Ok(())
    }

    fn text(&mut self, bytes: &[u8], depth: u8) -> Result<()> {
        if depth > 0 && bytes.len() >= MIN_VALUE {
            self.atom(bytes, depth)
        } else {
            self.literal(bytes, depth)
        }
    }

    fn content(&mut self, bytes: &[u8], depth: u8) -> Result<()> {
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if !is_base64(bytes[i]) {
                i += 1;
                continue;
            }
            let begin = i;
            while i < bytes.len() && is_base64(bytes[i]) {
                i += 1;
            }
            while i < bytes.len() && bytes[i] == b'=' {
                i += 1;
            }
            if i - begin < MIN_VALUE {
                continue;
            }
            let token = &bytes[begin..i];
            let url = token.iter().any(|c| *c == b'-' || *c == b'_');
            let no_pad = !token.ends_with(b"=");
            let flags =
                depth | BASE64 | if url { URL } else { 0 } | if no_pad { NO_PAD } else { 0 };
            let engine = engine(flags);
            if let Ok(binary) = engine.decode(token) {
                // Includes canonical padding bits, alphabet and padding policy.
                if engine.encode(&binary).as_bytes() == token {
                    self.text(&bytes[start..begin], depth)?;
                    self.atom(&binary, flags)?;
                    start = i;
                }
            }
        }
        self.text(&bytes[start..], depth)
    }

    fn scan(&mut self, bytes: &[u8], depth: u8) -> Result<()> {
        if depth == MAX_DEPTH {
            return self.content(bytes, depth);
        }
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != b'"' {
                i += 1;
                continue;
            }
            let begin = i;
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i = (i + 2).min(bytes.len()),
                    b'"' => break,
                    _ => i += 1,
                }
            }
            if i == bytes.len() {
                break;
            }
            i += 1;
            if i - begin < MIN_VALUE {
                continue;
            }
            let token = &bytes[begin..i];
            if let Ok(value) = serde_json::from_slice::<String>(token) {
                if serde_json::to_vec(&value)? == token {
                    self.content(&bytes[start..begin + 1], depth)?;
                    self.scan(value.as_bytes(), depth + 1)?;
                    self.literal(b"\"", depth)?;
                    start = i;
                }
            }
        }
        self.content(&bytes[start..], depth)
    }
}

fn is_base64(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'-' | b'_')
}

fn engine(flags: u8) -> &'static b64::GeneralPurpose {
    match (flags & URL != 0, flags & NO_PAD != 0) {
        (false, false) => &b64::STANDARD,
        (false, true) => &b64::STANDARD_NO_PAD,
        (true, false) => &b64::URL_SAFE,
        (true, true) => &b64::URL_SAFE_NO_PAD,
    }
}

/// No whole-file buffer. Oversized records are preserved in bounded raw slices.
pub fn encode(reader: impl Read, store: impl FnMut(&[u8]) -> Result<u32>) -> Result<Recipe> {
    encode_with_policy(reader, false, store)
}

pub fn encode_with_policy(
    reader: impl Read,
    small: bool,
    store: impl FnMut(&[u8]) -> Result<u32>,
) -> Result<Recipe> {
    let mut reader = BufReader::new(reader);
    let mut builder = Builder {
        store,
        small,
        operations: Vec::new(),
        literals: Vec::new(),
        literal_ops: Vec::new(),
    };
    let mut line = Vec::new();
    let mut hash = blake3::Hasher::new();
    let mut decoded_bytes = 0;
    loop {
        line.clear();
        let n = reader
            .by_ref()
            .take(MAX_LINE as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        hash.update(&line);
        decoded_bytes += n as u64;
        if n == MAX_LINE && line.last() != Some(&b'\n') {
            builder.literal(&line, 0)?;
        } else {
            builder.scan(&line, 0)?;
        }
    }
    builder.flush_literals()?;
    Ok(Recipe {
        operations: builder.operations,
        decoded_bytes,
        decoded_hash: hash.finalize().to_hex().to_string(),
        container: None,
    })
}

pub fn validate_flags(flags: u32) -> Result<u8> {
    ensure!(
        flags <= 63 && flags & 7 <= MAX_DEPTH as u32,
        "invalid structural transform"
    );
    ensure!(
        flags & BASE64 as u32 != 0 || flags & (URL | NO_PAD) as u32 == 0,
        "invalid base64 flags"
    );
    Ok(flags as u8)
}

/// The maximum expansion is bounded by one chunk and three JSON escape layers.
pub fn expand(bytes: &[u8], flags: u32) -> Result<Vec<u8>> {
    let flags = validate_flags(flags)?;
    let mut out = if flags & BASE64 != 0 {
        engine(flags).encode(bytes).into_bytes()
    } else {
        bytes.to_vec()
    };
    for _ in 0..flags & 7 {
        let mut escaped = Vec::with_capacity(out.len());
        for c in out {
            match c {
                b'"' => escaped.extend_from_slice(b"\\\""),
                b'\\' => escaped.extend_from_slice(b"\\\\"),
                b'\n' => escaped.extend_from_slice(b"\\n"),
                b'\r' => escaped.extend_from_slice(b"\\r"),
                b'\t' => escaped.extend_from_slice(b"\\t"),
                8 => escaped.extend_from_slice(b"\\b"),
                12 => escaped.extend_from_slice(b"\\f"),
                0..=31 => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    escaped.extend_from_slice(&[
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[(c >> 4) as usize],
                        HEX[(c & 15) as usize],
                    ]);
                }
                _ => escaped.push(c),
            }
        }
        out = escaped;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn roundtrip(input: &[u8]) {
        let mut chunks = Vec::new();
        let recipe = encode(input, |bytes| {
            chunks.push(bytes.to_vec());
            Ok(chunks.len() as u32 - 1)
        })
        .unwrap();
        let mut result = Vec::new();
        for [id, offset, len, flags] in recipe.operations {
            let part = &chunks[id as usize][offset as usize..(offset + len) as usize];
            result.extend_from_slice(&expand(part, flags).unwrap());
        }
        assert_eq!(result, input);
        assert_eq!(
            recipe.decoded_hash,
            blake3::hash(input).to_hex().to_string()
        );
    }
    #[test]
    fn lexical_roundtrips_preserve_spelling_and_binary() {
        for raw in [
            b"".as_slice(),
            b"{\"unknown\":1e+00, \"x\":\"\\u0061\\/\"}\r\n\xff\x00",
            b"{\"unfinished\":\"oops\\",
        ] {
            roundtrip(raw);
        }
        let text = "lines\\\"\n\t\u{0001}é𐀀".repeat(1000);
        let nested = serde_json::to_string(&serde_json::json!({"unknown": text})).unwrap();
        roundtrip(serde_json::to_string(&nested).unwrap().as_bytes());
        for engine in [
            &b64::STANDARD,
            &b64::STANDARD_NO_PAD,
            &b64::URL_SAFE,
            &b64::URL_SAFE_NO_PAD,
        ] {
            for len in [262143, 262144, 262145, 600001] {
                let bytes = (0..len).map(|i| (i % 256) as u8).collect::<Vec<_>>();
                let text = format!("{{\"data\":\"{}\"}}", engine.encode(bytes));
                roundtrip(text.as_bytes());
            }
        }
    }
}
