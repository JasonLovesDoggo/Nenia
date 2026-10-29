//! Exact baseline JPEG coefficient normalization. No inverse DCT or pixel loss.
//! Unsupported scans and noncanonical entropy encodings stay opaque at callers.
//! The encoder always proves that coefficients recreate the entire source file.
use anyhow::{Result, bail, ensure};
const MAX_BLOCKS: usize = 1_048_576;
#[derive(Clone)]
struct Huffman {
    codes: Vec<(u16, u8)>,
    counts: [usize; 17],
    first: [usize; 17],
    symbols: Vec<u8>,
}
impl Huffman {
    fn new(counts: &[u8], symbols: &[u8]) -> Result<Self> {
        ensure!(
            counts.len() == 16
                && counts.iter().map(|&n| n as usize).sum::<usize>() == symbols.len(),
            "JPEG Huffman table length"
        );
        let mut out = Self {
            codes: vec![(0, 0); 256],
            counts: [0; 17],
            first: [0; 17],
            symbols: symbols.to_vec(),
        };
        let mut code = 0;
        let mut offset = 0;
        for bits in 1..=16 {
            out.counts[bits] = counts[bits - 1] as usize;
            out.first[bits] = code;
            ensure!(
                code + out.counts[bits] <= 1 << bits,
                "oversubscribed JPEG tree"
            );
            for j in 0..out.counts[bits] {
                let symbol = symbols[offset + j] as usize;
                ensure!(out.codes[symbol].1 == 0, "duplicate JPEG symbol");
                out.codes[symbol] = ((code + j) as u16, bits as u8);
            }
            offset += out.counts[bits];
            code = (code + out.counts[bits]) << 1;
        }
        Ok(out)
    }
    fn read(&self, r: &mut Bits<'_>) -> Result<usize> {
        let mut code = 0;
        let mut offset = 0;
        for bits in 1..=16 {
            code = (code << 1) | r.read(1)?;
            if code >= self.first[bits] && code - self.first[bits] < self.counts[bits] {
                return Ok(self.symbols[offset + code - self.first[bits]] as usize);
            }
            offset += self.counts[bits];
        }
        bail!("invalid JPEG Huffman code")
    }
    fn write(&self, w: &mut Writer, symbol: usize) -> Result<()> {
        ensure!(
            symbol < 256 && self.codes[symbol].1 > 0,
            "absent JPEG symbol"
        );
        let (code, n) = self.codes[symbol];
        w.write(code as usize, n as usize)
    }
}
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    byte: u8,
    left: usize,
}
impl Bits<'_> {
    fn read(&mut self, n: usize) -> Result<usize> {
        ensure!(n <= 16, "JPEG bit count");
        let mut value = 0;
        for _ in 0..n {
            if self.left == 0 {
                ensure!(self.pos < self.data.len(), "truncated JPEG entropy");
                self.byte = self.data[self.pos];
                self.pos += 1;
                if self.byte == 255 {
                    ensure!(
                        self.data.get(self.pos) == Some(&0),
                        "unexpected JPEG marker"
                    );
                    self.pos += 1;
                }
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | ((self.byte as usize >> self.left) & 1);
        }
        Ok(value)
    }
    fn align(&mut self) -> Result<()> {
        let n = self.left;
        ensure!(self.read(n)? == (1 << n) - 1, "noncanonical JPEG padding");
        Ok(())
    }
    fn restart(&mut self, id: usize) -> Result<()> {
        self.align()?;
        ensure!(
            self.data.get(self.pos..self.pos + 2) == Some(&[255, 208 + (id % 8) as u8]),
            "JPEG restart marker"
        );
        self.pos += 2;
        Ok(())
    }
}
struct Writer {
    out: Vec<u8>,
    byte: u8,
    used: usize,
}
impl Writer {
    fn write(&mut self, value: usize, n: usize) -> Result<()> {
        ensure!(
            n <= 16 && value < 1 << n && self.out.len() < 128 * 1024 * 1024,
            "JPEG output bounds"
        );
        for shift in (0..n).rev() {
            self.byte = (self.byte << 1) | ((value >> shift) & 1) as u8;
            self.used += 1;
            if self.used == 8 {
                self.out.push(self.byte);
                if self.byte == 255 {
                    self.out.push(0);
                }
                self.byte = 0;
                self.used = 0;
            }
        }
        Ok(())
    }
    fn align(&mut self) -> Result<()> {
        let n = (8 - self.used) % 8;
        self.write((1 << n) - 1, n)
    }
    fn restart(&mut self, id: usize) -> Result<()> {
        self.align()?;
        self.out.extend([255, 208 + (id % 8) as u8]);
        Ok(())
    }
}
struct Component {
    id: u8,
    h: usize,
    v: usize,
    dc: usize,
    ac: usize,
}
struct Layout {
    components: Vec<Component>,
    order: Vec<usize>,
    dc: Vec<Option<Huffman>>,
    ac: Vec<Option<Huffman>>,
    mcus: usize,
    restart: usize,
    header_len: usize,
}
fn u16be(b: &[u8]) -> usize {
    u16::from_be_bytes([b[0], b[1]]) as usize
}
fn layout(data: &[u8]) -> Result<Layout> {
    ensure!(data.len() >= 4 && data[..2] == [255, 216], "JPEG SOI");
    let mut out = Layout {
        components: Vec::new(),
        order: Vec::new(),
        dc: vec![None; 4],
        ac: vec![None; 4],
        mcus: 0,
        restart: 0,
        header_len: 0,
    };
    let mut pos = 2;
    let mut width = 0;
    let mut height = 0;
    loop {
        ensure!(data.get(pos) == Some(&255), "JPEG marker prefix");
        pos += 1;
        while data.get(pos) == Some(&255) {
            pos += 1;
        }
        let marker = *data
            .get(pos)
            .ok_or_else(|| anyhow::anyhow!("truncated JPEG marker"))?;
        pos += 1;
        ensure!(pos + 2 <= data.len(), "truncated JPEG segment");
        let size = u16be(&data[pos..pos + 2]);
        ensure!(size >= 2 && pos + size <= data.len(), "JPEG segment bounds");
        let body = &data[pos + 2..pos + size];
        pos += size;
        match marker {
            0xc0 => {
                ensure!(
                    body.len() >= 6 && body[0] == 8 && out.components.is_empty(),
                    "baseline JPEG frame"
                );
                height = u16be(&body[1..3]);
                width = u16be(&body[3..5]);
                let count = body[5] as usize;
                ensure!(
                    (1..=4).contains(&count)
                        && body.len() == 6 + count * 3
                        && width > 0
                        && height > 0
                        && width <= 16384
                        && height <= 16384,
                    "JPEG dimensions"
                );
                for c in body[6..].chunks_exact(3) {
                    let h = (c[1] >> 4) as usize;
                    let v = (c[1] & 15) as usize;
                    ensure!(
                        (1..=4).contains(&h)
                            && (1..=4).contains(&v)
                            && !out.components.iter().any(|old| old.id == c[0]),
                        "JPEG component layout"
                    );
                    out.components.push(Component {
                        id: c[0],
                        h,
                        v,
                        dc: 0,
                        ac: 0,
                    });
                }
            }
            0xc4 => {
                let mut b = body;
                while !b.is_empty() {
                    ensure!(b.len() >= 17, "JPEG DHT size");
                    let kind = b[0] >> 4;
                    let id = (b[0] & 15) as usize;
                    ensure!(kind <= 1 && id < 4, "JPEG DHT identifier");
                    let n = b[1..17].iter().map(|&v| v as usize).sum::<usize>();
                    ensure!(n <= 256 && b.len() >= 17 + n, "JPEG DHT symbols");
                    let table = Huffman::new(&b[1..17], &b[17..17 + n])?;
                    if kind == 0 {
                        out.dc[id] = Some(table)
                    } else {
                        out.ac[id] = Some(table)
                    }
                    b = &b[17 + n..];
                }
            }
            0xdd => {
                ensure!(body.len() == 2, "JPEG restart interval");
                out.restart = u16be(body);
            }
            0xda => {
                ensure!(
                    !body.is_empty() && !out.components.is_empty(),
                    "missing JPEG frame"
                );
                let n = body[0] as usize;
                ensure!(
                    n == out.components.len()
                        && body.len() == 1 + n * 2 + 3
                        && body[body.len() - 3..] == [0, 63, 0],
                    "unsupported JPEG scan"
                );
                for c in body[1..1 + n * 2].chunks_exact(2) {
                    let i = out
                        .components
                        .iter()
                        .position(|v| v.id == c[0])
                        .ok_or_else(|| anyhow::anyhow!("unknown JPEG scan component"))?;
                    ensure!(!out.order.contains(&i), "duplicate scan component");
                    let dc = (c[1] >> 4) as usize;
                    let ac = (c[1] & 15) as usize;
                    ensure!(
                        dc < 4 && ac < 4 && out.dc[dc].is_some() && out.ac[ac].is_some(),
                        "missing JPEG Huffman table"
                    );
                    out.components[i].dc = dc;
                    out.components[i].ac = ac;
                    out.order.push(i);
                }
                if n == 1 {
                    ensure!(
                        out.components[0].h == 1 && out.components[0].v == 1,
                        "noninterleaved sampling"
                    );
                }
                let max_h = out.components.iter().map(|c| c.h).max().unwrap();
                let max_v = out.components.iter().map(|c| c.v).max().unwrap();
                out.mcus = width.div_ceil(max_h * 8) * height.div_ceil(max_v * 8);
                let blocks = out.components.iter().map(|c| c.h * c.v).sum::<usize>();
                ensure!(
                    blocks <= 10 && out.mcus * blocks <= MAX_BLOCKS,
                    "JPEG coefficient budget"
                );
                out.header_len = pos;
                return Ok(out);
            }
            0xdb | 0xfe | 0xe0..=0xef => {}
            _ => bail!("unsupported JPEG marker"),
        }
    }
}
fn extend(value: usize, n: usize) -> i32 {
    if n == 0 {
        0
    } else if value < 1 << (n - 1) {
        value as i32 - ((1i32 << n) - 1)
    } else {
        value as i32
    }
}
fn magnitude(value: i32) -> (usize, usize) {
    let n = (32 - value.unsigned_abs().leading_zeros()) as usize;
    (
        n,
        if value < 0 {
            (value + ((1i32 << n) - 1)) as usize
        } else {
            value as usize
        },
    )
}
pub struct Coefficients {
    pub header: Vec<u8>,
    pub tail: Vec<u8>,
    pub blocks: Vec<[i16; 64]>,
}
pub fn decode(data: &[u8]) -> Result<Coefficients> {
    ensure!(data.len() <= 128 * 1024 * 1024, "JPEG source budget");
    let layout = layout(data)?;
    let mut r = Bits {
        data,
        pos: layout.header_len,
        byte: 0,
        left: 0,
    };
    let mut predictors = [0i32; 4];
    let mut blocks = Vec::new();
    for mcu in 0..layout.mcus {
        if mcu > 0 && layout.restart > 0 && mcu % layout.restart == 0 {
            r.restart(mcu / layout.restart - 1)?;
            predictors = [0; 4];
        }
        for &id in &layout.order {
            let c = &layout.components[id];
            let dc = layout.dc[c.dc].as_ref().unwrap();
            let ac = layout.ac[c.ac].as_ref().unwrap();
            for _ in 0..c.h * c.v {
                let mut block = [0i16; 64];
                let n = dc.read(&mut r)?;
                ensure!(n <= 11, "JPEG DC category");
                predictors[id] += extend(r.read(n)?, n);
                block[0] = predictors[id].try_into()?;
                let mut k = 1;
                while k < 64 {
                    let symbol = ac.read(&mut r)?;
                    if symbol == 0 {
                        break;
                    }
                    let run = symbol >> 4;
                    let n = symbol & 15;
                    if n == 0 {
                        ensure!(run == 15 && k + 16 <= 64, "JPEG zero run");
                        k += 16;
                        continue;
                    }
                    ensure!(n <= 10 && k + run < 64, "JPEG AC bounds");
                    k += run;
                    block[k] = extend(r.read(n)?, n).try_into()?;
                    k += 1;
                }
                blocks.push(block);
            }
        }
    }
    r.align()?;
    ensure!(
        data.get(r.pos..r.pos + 2) == Some(&[255, 217]),
        "JPEG trailing scan"
    );
    Ok(Coefficients {
        header: data[..layout.header_len].to_vec(),
        tail: data[r.pos..].to_vec(),
        blocks,
    })
}
pub fn encode(data: &Coefficients) -> Result<Vec<u8>> {
    let layout = layout(&data.header)?;
    let expected = layout.mcus * layout.components.iter().map(|c| c.h * c.v).sum::<usize>();
    ensure!(data.blocks.len() == expected, "JPEG block count");
    let mut w = Writer {
        out: data.header.clone(),
        byte: 0,
        used: 0,
    };
    let mut predictors = [0i32; 4];
    let mut index = 0;
    for mcu in 0..layout.mcus {
        if mcu > 0 && layout.restart > 0 && mcu % layout.restart == 0 {
            w.restart(mcu / layout.restart - 1)?;
            predictors = [0; 4];
        }
        for &id in &layout.order {
            let c = &layout.components[id];
            let dc = layout.dc[c.dc].as_ref().unwrap();
            let ac = layout.ac[c.ac].as_ref().unwrap();
            for _ in 0..c.h * c.v {
                let block = &data.blocks[index];
                index += 1;
                let value = block[0] as i32;
                let (n, bits) = magnitude(value - predictors[id]);
                ensure!(n <= 11, "DC encode category");
                dc.write(&mut w, n)?;
                w.write(bits, n)?;
                predictors[id] = value;
                let mut k = 1;
                while k < 64 {
                    let Some(next) = (k..64).find(|&j| block[j] != 0) else {
                        ac.write(&mut w, 0)?;
                        break;
                    };
                    let mut run = next - k;
                    while run >= 16 {
                        ac.write(&mut w, 240)?;
                        run -= 16;
                    }
                    let (n, bits) = magnitude(block[next] as i32);
                    ensure!(n <= 10, "AC encode category");
                    ac.write(&mut w, (run << 4) | n)?;
                    w.write(bits, n)?;
                    k = next + 1;
                }
            }
        }
    }
    w.align()?;
    w.out.extend_from_slice(&data.tail);
    Ok(w.out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn magnitude_is_invertible() {
        for value in -2047..=2047 {
            let (n, bits) = magnitude(value);
            assert_eq!(extend(bits, n), value);
        }
    }
    #[test]
    fn rejects_incomplete_headers() {
        for data in [
            b"".as_slice(),
            &[255, 216],
            &[255, 216, 255, 192, 0, 1],
            &[255, 216, 255, 196, 255, 255],
        ] {
            assert!(decode(data).is_err());
        }
    }
}
