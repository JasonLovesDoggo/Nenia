//! Exact DEFLATE reconstruction without identifying the original compressor.
//! Literal bytes are implicit in the separately stored plaintext. Only original
//! block headers, match decisions, and padding survive in the recipe. The match
//! columns compress independently; even the alternate 258 encoding is retained.
use anyhow::{Result, bail, ensure};

const LIMIT: usize = 128 * 1024 * 1024;
const LENGTH: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LE: [usize; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DE: [usize; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Bits<'a> {
    fn read(&mut self, n: usize) -> Result<usize> {
        ensure!(
            n <= 16 && self.pos + n <= self.data.len() * 8,
            "truncated bits"
        );
        let value = self.peek(n);
        self.pos += n;
        Ok(value)
    }
    fn peek(&self, n: usize) -> usize {
        let byte = self.pos / 8;
        let mut value = 0usize;
        for j in 0..3 {
            value |= (*self.data.get(byte + j).unwrap_or(&0) as usize) << (j * 8);
        }
        (value >> (self.pos % 8)) & ((1 << n) - 1)
    }
}
#[derive(Default)]
struct Writer {
    data: Vec<u8>,
    pos: usize,
}
impl Writer {
    fn write(&mut self, value: usize, n: usize) -> Result<()> {
        ensure!(
            n <= 16 && value < (1 << n) && self.pos + n <= LIMIT * 8,
            "bit output limit"
        );
        self.data.resize((self.pos + n).div_ceil(8), 0);
        for i in 0..n {
            self.data[(self.pos + i) / 8] |= (((value >> i) & 1) as u8) << ((self.pos + i) % 8);
        }
        self.pos += n;
        Ok(())
    }
    fn copy(&mut self, data: &[u8], start: usize, n: usize) -> Result<()> {
        let mut r = Bits { data, pos: start };
        let mut remaining = n;
        while remaining > 0 {
            let count = remaining.min(16);
            self.write(r.read(count)?, count)?;
            remaining -= count;
        }
        Ok(())
    }
}
struct Huffman {
    table: Vec<(u16, u8)>,
    codes: Vec<(usize, usize)>,
}
impl Huffman {
    fn new(lengths: &[usize]) -> Result<Self> {
        let mut counts = [0usize; 16];
        for &len in lengths {
            ensure!(len <= 15, "long Huffman code");
            counts[len] += 1;
        }
        counts[0] = 0;
        let mut next = [0usize; 16];
        for n in 1..16 {
            next[n] = (next[n - 1] + counts[n - 1]) << 1;
        }
        ensure!(
            next[15] + counts[15] <= 32768,
            "oversubscribed Huffman tree"
        );
        let mut table = vec![(0, 0); 32768];
        let mut codes = Vec::with_capacity(lengths.len());
        for (symbol, &len) in lengths.iter().enumerate() {
            if len == 0 {
                codes.push((0, 0));
                continue;
            }
            let code = next[len].reverse_bits() >> (usize::BITS as usize - len);
            next[len] += 1;
            codes.push((code, len));
            for i in (code..32768).step_by(1 << len) {
                table[i] = (symbol as u16, len as u8);
            }
        }
        Ok(Self { table, codes })
    }
    fn read(&self, r: &mut Bits<'_>) -> Result<usize> {
        let (symbol, n) = self.table[r.peek(15)];
        ensure!(n != 0, "absent Huffman code");
        r.read(n as usize)?;
        Ok(symbol as usize)
    }
    fn write(&self, w: &mut Writer, symbol: usize) -> Result<()> {
        let &(code, n) = self
            .codes
            .get(symbol)
            .ok_or_else(|| anyhow::anyhow!("invalid symbol"))?;
        ensure!(n != 0, "absent symbol");
        w.write(code, n)
    }
}
fn trees(r: &mut Bits<'_>, kind: usize) -> Result<(Huffman, Huffman)> {
    if kind == 1 {
        let lengths: Vec<_> = (0..288)
            .map(|i| match i {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            })
            .collect();
        return Ok((Huffman::new(&lengths)?, Huffman::new(&[5; 32])?));
    }
    ensure!(kind == 2, "invalid compressed block");
    let nl = r.read(5)? + 257;
    let nd = r.read(5)? + 1;
    let nc = r.read(4)? + 4;
    ensure!(nl <= 286, "reserved literal codes");
    // RFC 1951's deliberately interleaved code-length alphabet.
    let order = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut cl = [0; 19];
    for &i in &order[..nc] {
        cl[i] = r.read(3)?;
    }
    let codes = Huffman::new(&cl)?;
    let mut lengths = Vec::new();
    while lengths.len() < nl + nd {
        let symbol = codes.read(r)?;
        let (value, count) = match symbol {
            0..=15 => (symbol, 1),
            16 => (
                *lengths
                    .last()
                    .ok_or_else(|| anyhow::anyhow!("no previous code"))?,
                r.read(2)? + 3,
            ),
            17 => (0, r.read(3)? + 3),
            18 => (0, r.read(7)? + 11),
            _ => bail!("invalid length symbol"),
        };
        ensure!(lengths.len() + count <= nl + nd, "excess lengths");
        lengths.resize(lengths.len() + count, value);
    }
    ensure!(lengths[256] != 0, "missing end symbol");
    Ok((Huffman::new(&lengths[..nl])?, Huffman::new(&lengths[nl..])?))
}
fn put(out: &mut Vec<u8>, mut n: usize) {
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn get(input: &mut &[u8]) -> Result<usize> {
    let mut n = 0usize;
    for shift in (0..35).step_by(7) {
        ensure!(!input.is_empty(), "truncated recipe integer");
        let b = input[0];
        *input = &input[1..];
        n |= ((b & 127) as usize) << shift;
        if b < 128 {
            ensure!(n <= LIMIT, "recipe integer limit");
            return Ok(n);
        }
    }
    bail!("overlong recipe integer")
}
fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    ensure!(n <= input.len(), "truncated recipe bytes");
    let out = &input[..n];
    *input = &input[n..];
    Ok(out)
}
struct Block {
    head: Vec<u8>,
    bits: usize,
    size: usize,
    runs: Vec<usize>,
    lengths: Vec<usize>,
    distances: Vec<usize>,
}
pub struct Encoded {
    pub plain: Vec<u8>,
    pub recipe: Vec<u8>,
    pub alternate_lengths: usize,
}
pub fn encode(data: &[u8]) -> Result<Encoded> {
    ensure!(data.len() <= LIMIT, "compressed input limit");
    let mut r = Bits { data, pos: 0 };
    let mut plain = Vec::new();
    let mut blocks = Vec::new();
    let mut alternate_lengths = 0;
    loop {
        ensure!(blocks.len() < 65536, "block count limit");
        let begin = r.pos;
        let final_block = r.read(1)? != 0;
        let kind = r.read(2)?;
        let plain_begin = plain.len();
        let mut b = Block {
            head: Vec::new(),
            bits: 0,
            size: 0,
            runs: Vec::new(),
            lengths: Vec::new(),
            distances: Vec::new(),
        };
        let mut header = Writer::default();
        if kind == 0 {
            r.read((8 - r.pos % 8) % 8)?;
            let len = r.read(16)?;
            ensure!(len ^ r.read(16)? == 65535, "stored block length mismatch");
            b.bits = r.pos - begin;
            header.copy(data, begin, b.bits)?;
            ensure!(plain.len() + len <= LIMIT, "plaintext limit");
            for _ in 0..len {
                plain.push(r.read(8)? as u8);
            }
        } else {
            let (lit, dist) = trees(&mut r, kind)?;
            b.bits = r.pos - begin;
            header.copy(data, begin, b.bits)?;
            let mut run = 0;
            loop {
                let symbol = lit.read(&mut r)?;
                if symbol < 256 {
                    ensure!(plain.len() < LIMIT, "plaintext limit");
                    plain.push(symbol as u8);
                    run += 1;
                } else if symbol == 256 {
                    break;
                } else {
                    ensure!(symbol <= 285, "reserved length symbol");
                    let index = symbol - 257;
                    let len = LENGTH[index] + r.read(LE[index])?;
                    let ds = dist.read(&mut r)?;
                    ensure!(ds < 30, "reserved distance symbol");
                    let distance = DIST[ds] + r.read(DE[ds])?;
                    ensure!(
                        distance <= plain.len() && plain.len() + len <= LIMIT,
                        "match bounds"
                    );
                    b.runs.push(run);
                    run = 0;
                    let alternate = len == 258 && symbol == 284;
                    alternate_lengths += usize::from(alternate);
                    b.lengths.push(if alternate { 259 } else { len });
                    b.distances.push(distance);
                    for _ in 0..len {
                        plain.push(plain[plain.len() - distance]);
                    }
                }
            }
        }
        b.head = header.data;
        b.size = plain.len() - plain_begin;
        blocks.push(b);
        if final_block {
            break;
        }
    }
    let tail = data.len() * 8 - r.pos;
    ensure!(tail < 8, "trailing DEFLATE data");
    let mut recipe = vec![1];
    put(&mut recipe, blocks.len());
    for b in blocks {
        put(&mut recipe, b.bits);
        recipe.extend(b.head);
        put(&mut recipe, b.size);
        put(&mut recipe, b.runs.len());
        for column in [&b.runs, &b.lengths, &b.distances] {
            for &v in column {
                put(&mut recipe, v);
            }
        }
    }
    put(&mut recipe, tail);
    put(&mut recipe, r.read(tail)?);
    Ok(Encoded {
        plain,
        recipe,
        alternate_lengths,
    })
}
pub fn decode(plain: &[u8], mut recipe: &[u8]) -> Result<Vec<u8>> {
    ensure!(plain.len() <= LIMIT, "plaintext size");
    let version = take(&mut recipe, 1)?[0];
    ensure!(matches!(version, 1 | 2), "recipe version");
    let mut witness = Witness::new();
    let blocks = get(&mut recipe)?;
    ensure!((1..=65536).contains(&blocks), "block count limit");
    let mut w = Writer::default();
    let mut pos = 0;
    for index in 0..blocks {
        let bits = get(&mut recipe)?;
        ensure!((3..=65536).contains(&bits), "header size limit");
        let header = take(&mut recipe, bits.div_ceil(8))?;
        let mut h = Bits {
            data: header,
            pos: 0,
        };
        ensure!(
            (h.read(1)? != 0) == (index + 1 == blocks),
            "final block mismatch"
        );
        let kind = h.read(2)?;
        let size = get(&mut recipe)?;
        ensure!(size <= plain.len() - pos, "plaintext block bounds");
        let end = pos + size;
        let count = get(&mut recipe)?;
        ensure!(count <= size / 3, "match count limit");
        let mut runs = Vec::with_capacity(count);
        let mut lengths = Vec::with_capacity(count);
        let mut distances = Vec::with_capacity(count);
        for column in [&mut runs, &mut lengths, &mut distances] {
            for _ in 0..count {
                column.push(get(&mut recipe)?);
            }
        }
        let start_bit = w.pos;
        w.copy(header, 0, bits)?;
        if kind == 0 {
            ensure!(count == 0, "stored block has matches");
            h.read((8 - (start_bit + 3) % 8) % 8)?;
            ensure!(
                h.read(16)? == size && h.read(16)? ^ size == 65535 && h.pos == bits,
                "stored header mismatch"
            );
            for &v in &plain[pos..end] {
                w.write(v as usize, 8)?;
            }
            pos = end;
        } else {
            let (lit, dist) = trees(&mut h, kind)?;
            ensure!(h.pos == bits, "compressed header mismatch");
            for j in 0..count {
                ensure!(runs[j] <= end - pos, "literal run bounds");
                for &v in &plain[pos..pos + runs[j]] {
                    lit.write(&mut w, v as usize)?;
                }
                pos += runs[j];
                let stored_len = lengths[j];
                ensure!((3..=259).contains(&stored_len), "match length bounds");
                let len = stored_len.min(258);
                let distance = if version == 2 {
                    witness.advance(plain, pos);
                    witness.distance(plain, pos, distances[j])?
                } else {
                    distances[j]
                };
                ensure!(
                    (1..=32768).contains(&distance) && distance <= pos && len <= end - pos,
                    "match distance bounds"
                );
                ensure!(
                    (0..len).all(|i| plain[pos + i] == plain[pos + i - distance]),
                    "match differs from plaintext"
                );
                let ls = if stored_len == 259 {
                    27
                } else {
                    LENGTH.iter().rposition(|&n| n <= len).unwrap()
                };
                let ds = DIST.iter().rposition(|&n| n <= distance).unwrap();
                lit.write(&mut w, ls + 257)?;
                w.write(len - LENGTH[ls], LE[ls])?;
                dist.write(&mut w, ds)?;
                w.write(distance - DIST[ds], DE[ds])?;
                pos += len;
            }
            for &v in &plain[pos..end] {
                lit.write(&mut w, v as usize)?;
            }
            pos = end;
            lit.write(&mut w, 256)?;
        }
    }
    ensure!(pos == plain.len(), "unused plaintext");
    let tail = get(&mut recipe)?;
    let value = get(&mut recipe)?;
    ensure!(
        tail < 8 && (w.pos + tail) % 8 == 0 && recipe.is_empty(),
        "trailing recipe data"
    );
    w.write(value, tail)?;
    Ok(w.data)
}

/// Replace physical distances with ranks in a deterministic, 32 KiB history.
/// No compressor identification, training data, or unbounded search is required.
/// Literal bytes already present in plaintext supply the matching context.
pub fn rank_recipe(plain: &[u8], mut recipe: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        plain.len() <= LIMIT && take(&mut recipe, 1)? == [1],
        "recipe version/size"
    );
    let count = get(&mut recipe)?;
    ensure!(count <= 65536, "block limit");
    let mut out = vec![2];
    put(&mut out, count);
    let mut witness = Witness::new();
    let mut pos = 0;
    for _ in 0..count {
        let bits = get(&mut recipe)?;
        put(&mut out, bits);
        out.extend_from_slice(take(&mut recipe, bits.div_ceil(8))?);
        let size = get(&mut recipe)?;
        put(&mut out, size);
        ensure!(size <= plain.len() - pos, "block bounds");
        let end = pos + size;
        let matches = get(&mut recipe)?;
        put(&mut out, matches);
        ensure!(matches <= size / 3, "match count");
        let mut runs = Vec::with_capacity(matches);
        let mut lengths = Vec::with_capacity(matches);
        for column in [&mut runs, &mut lengths] {
            for _ in 0..matches {
                let v = get(&mut recipe)?;
                column.push(v);
                put(&mut out, v);
            }
        }
        for i in 0..matches {
            let distance = get(&mut recipe)?;
            ensure!(runs[i] <= end - pos, "literal bounds");
            pos += runs[i];
            ensure!(
                distance > 0 && distance <= pos && distance <= 32768,
                "distance bounds"
            );
            witness.advance(plain, pos);
            put(&mut out, witness.rank(plain, pos, distance));
            let len = lengths[i].min(258);
            ensure!(
                (3..=259).contains(&lengths[i]) && len <= end - pos,
                "length bounds"
            );
            pos += len;
        }
        pos = end;
    }
    ensure!(pos == plain.len(), "unused plaintext");
    out.extend_from_slice(recipe);
    Ok(out)
}
struct Witness {
    heads: Vec<usize>,
    previous: Vec<usize>,
    inserted: usize,
}
impl Witness {
    fn new() -> Self {
        Self {
            heads: vec![usize::MAX; 65536],
            previous: vec![usize::MAX; 32768],
            inserted: 0,
        }
    }
    fn key(plain: &[u8], pos: usize) -> usize {
        let v = plain[pos] as u32 | (plain[pos + 1] as u32) << 8 | (plain[pos + 2] as u32) << 16;
        (v.wrapping_mul(0x9e3779b1) >> 16) as usize
    }
    fn advance(&mut self, plain: &[u8], pos: usize) {
        while self.inserted < pos && self.inserted + 2 < plain.len() {
            let key = Self::key(plain, self.inserted);
            self.previous[self.inserted & 32767] = self.heads[key];
            self.heads[key] = self.inserted;
            self.inserted += 1;
        }
    }
    fn rank(&self, plain: &[u8], pos: usize, distance: usize) -> usize {
        let mut candidate = self.heads[Self::key(plain, pos)];
        for rank in 0..64 {
            if candidate >= pos || pos - candidate > 32768 {
                break;
            }
            if pos - candidate == distance {
                return rank;
            }
            candidate = self.previous[candidate & 32767];
        }
        distance + 64
    }
    fn distance(&self, plain: &[u8], pos: usize, rank: usize) -> Result<usize> {
        if rank >= 64 {
            return Ok(rank - 64);
        }
        ensure!(pos + 2 < plain.len(), "match prefix bounds");
        let mut candidate = self.heads[Self::key(plain, pos)];
        for i in 0..=rank {
            ensure!(
                candidate < pos && pos - candidate <= 32768,
                "witness history bounds"
            );
            if i == rank {
                return Ok(pos - candidate);
            }
            candidate = self.previous[candidate & 32767];
        }
        unreachable!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn preserves_alternate_length_and_nonzero_padding() {
        let mut header = Bits { data: &[], pos: 0 };
        let (lit, dist) = trees(&mut header, 1).unwrap();
        let mut w = Writer::default();
        w.write(1, 1).unwrap();
        w.write(1, 2).unwrap();
        lit.write(&mut w, 65).unwrap();
        lit.write(&mut w, 284).unwrap();
        w.write(31, 5).unwrap();
        dist.write(&mut w, 0).unwrap();
        lit.write(&mut w, 256).unwrap();
        let padding = (8 - w.pos % 8) % 8;
        w.write((1 << padding) - 1, padding).unwrap();
        let encoded = encode(&w.data).unwrap();
        assert_eq!(encoded.alternate_lengths, 1);
        assert_eq!(encoded.plain, vec![65; 259]);
        assert_eq!(decode(&encoded.plain, &encoded.recipe).unwrap(), w.data);
        assert_eq!(
            decode(
                &encoded.plain,
                &rank_recipe(&encoded.plain, &encoded.recipe).unwrap()
            )
            .unwrap(),
            w.data
        );
    }
    #[test]
    fn handles_incomplete_fdeflate_tree_and_stored_block_padding() {
        let text = b"pixel bytes with a zero run\0\0\0\0\0\0\0\0".repeat(100);
        let zlib = fdeflate::compress_to_vec(&text);
        let encoded = encode(&zlib[2..zlib.len() - 4]).unwrap();
        assert_eq!(encoded.plain, text);
        assert_eq!(
            decode(&text, &encoded.recipe).unwrap(),
            zlib[2..zlib.len() - 4]
        );
        let raw = [0b11111001, 3, 0, 252, 255, 1, 2, 3];
        let encoded = encode(&raw).unwrap();
        assert_eq!(encoded.plain, [1, 2, 3]);
        assert_eq!(decode(&encoded.plain, &encoded.recipe).unwrap(), raw);
    }
    #[test]
    fn round_trip_encoder_levels_and_truncations() {
        // Correctness fixture only; never contributes benchmark measurements.
        let text = b"literal then repetition repetition repetition\0\xff".repeat(200);
        for level in [0, 1, 6, 9] {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
            encoder.write_all(&text).unwrap();
            let data = encoder.finish().unwrap();
            let parsed = encode(&data).unwrap();
            assert_eq!(parsed.plain, text);
            assert_eq!(decode(&text, &parsed.recipe).unwrap(), data);
            let ranked = rank_recipe(&text, &parsed.recipe).unwrap();
            assert_eq!(decode(&text, &ranked).unwrap(), data);
            assert!(decode(&text, &parsed.recipe[..parsed.recipe.len() - 1]).is_err());
            assert!(encode(&data[..data.len() - 1]).is_err());
        }
    }
}
