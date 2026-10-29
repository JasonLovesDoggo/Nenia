//! Private real-PNG diagnostic. Never emits source content or identifiers.
use anyhow::{Result, ensure};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Read,
    time::Instant,
};
struct Stream {
    seen: HashSet<blake3::Hash>,
    pack: Vec<u8>,
    bytes: u64,
    raw: u64,
    chunks: u64,
}
impl Stream {
    fn new() -> Self {
        Self {
            seen: HashSet::new(),
            pack: Vec::new(),
            bytes: 0,
            raw: 0,
            chunks: 0,
        }
    }
    fn flush(&mut self) -> Result<()> {
        if !self.pack.is_empty() {
            self.bytes += zstd::stream::encode_all(self.pack.as_slice(), 9)?.len() as u64;
            self.pack.clear();
        }
        Ok(())
    }
    fn add(&mut self, data: &[u8]) -> Result<()> {
        let mut start = 0;
        let mut hash = 0u64;
        for (i, &b) in data.iter().enumerate() {
            let mut v = (b as u64).wrapping_add(0x9e3779b97f4a7c15);
            v = (v ^ (v >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            v = (v ^ (v >> 27)).wrapping_mul(0x94d049bb133111eb);
            v ^= v >> 31;
            hash = (hash << 1).wrapping_add(v);
            let len = i + 1 - start;
            if len >= 65536 || (len >= 1024 && hash < u64::MAX / 3072) {
                self.chunk(&data[start..i + 1])?;
                start = i + 1;
                hash = 0;
            }
        }
        if start < data.len() {
            self.chunk(&data[start..])?;
        }
        Ok(())
    }
    fn chunk(&mut self, data: &[u8]) -> Result<()> {
        if self.seen.insert(blake3::hash(data)) {
            if self.pack.len() + data.len() > 8 * 1024 * 1024 {
                self.flush()?;
            }
            self.pack.extend_from_slice(data);
            self.raw += data.len() as u64;
            self.chunks += 1;
        }
        Ok(())
    }
}
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let a = a as i32;
    let b = b as i32;
    let c = c as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        a as u8
    } else if pb <= pc {
        b as u8
    } else {
        c as u8
    }
}
fn unfilter(raw: &[u8], row: usize, bpp: usize) -> Result<Vec<u8>> {
    ensure!(raw.len() % (row + 1) == 0, "invalid rows");
    let height = raw.len() / (row + 1);
    let mut pixels = vec![0u8; row * height];
    for y in 0..height {
        let filter = raw[y * (row + 1)];
        ensure!(filter <= 4, "invalid filter");
        for x in 0..row {
            let a = if x >= bpp {
                pixels[y * row + x - bpp]
            } else {
                0
            };
            let b = if y > 0 { pixels[(y - 1) * row + x] } else { 0 };
            let c = if x >= bpp && y > 0 {
                pixels[(y - 1) * row + x - bpp]
            } else {
                0
            };
            let pred = match filter {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((a as u16 + b as u16) / 2) as u8,
                4 => paeth(a, b, c),
                _ => unreachable!(),
            };
            pixels[y * row + x] = raw[y * (row + 1) + 1 + x].wrapping_add(pred);
        }
    }
    Ok(pixels)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 3,
        "usage: media-screen PRIVATE_CONCAT_PNG AGGREGATE_REPORT"
    );
    let mut f = File::open(&args[1])?;
    let mut filtered = Stream::new();
    let mut pixels = Stream::new();
    let mut original = Stream::new();
    let mut corrections = Stream::new();
    let mut aux = Stream::new();
    let mut count = 0;
    let mut native = 0;
    let mut corrected = 0;
    let mut failed = 0;
    let mut inflated = 0;
    let start = Instant::now();
    loop {
        let mut sig = [0u8; 8];
        if f.read(&mut sig)? == 0 {
            break;
        }
        ensure!(&sig == b"\x89PNG\r\n\x1a\n", "bad concat PNG");
        let mut file = sig.to_vec();
        let mut idat = Vec::new();
        let mut header = Vec::new();
        let mut side = Vec::new();
        loop {
            let mut head = [0u8; 8];
            f.read_exact(&mut head)?;
            let len = u32::from_be_bytes(head[..4].try_into()?) as usize;
            ensure!(len < 64 * 1024 * 1024, "chunk too large");
            let mut data = vec![0; len + 4];
            f.read_exact(&mut data)?;
            file.extend_from_slice(&head);
            file.extend_from_slice(&data);
            if &head[4..] == b"IHDR" {
                header = data[..len].to_vec();
            }
            if &head[4..] == b"IDAT" {
                idat.extend_from_slice(&data[..len]);
                side.extend_from_slice(&head);
                side.extend_from_slice(&data[len..]);
            } else {
                side.extend_from_slice(&head);
                side.extend_from_slice(&data);
            }
            if &head[4..] == b"IEND" {
                break;
            }
        }
        count += 1;
        native += file.len();
        original.add(&file)?;
        if header.len() != 13 || header[12] != 0 {
            failed += 1;
            continue;
        }
        let width = u32::from_be_bytes(header[..4].try_into()?) as usize;
        let depth = header[8] as usize;
        let channels = match header[9] {
            0 => 1,
            2 => 3,
            3 => 1,
            4 => 2,
            6 => 4,
            _ => 0,
        };
        let row = (width * depth * channels).div_ceil(8);
        let bpp = (depth * channels).div_ceil(8);
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(idat.as_slice())
            .take(128 * 1024 * 1024)
            .read_to_end(&mut raw)?;
        inflated += raw.len();
        filtered.add(&raw)?;
        pixels.add(&unfilter(&raw, row, bpp)?)?;
        let filters: Vec<_> = raw.chunks(row + 1).map(|r| r[0]).collect();
        side.extend_from_slice(&filters);
        aux.add(&side)?;
        match preflate_rs::preflate_whole_deflate_stream(
            &idat[2..idat.len() - 4],
            &preflate_rs::PreflateConfig {
                plain_text_limit: 128 * 1024 * 1024,
                verify_compression: true,
                ..Default::default()
            },
        ) {
            Ok((data, text)) => {
                ensure!(text.text() == raw, "plaintext mismatch");
                ensure!(data.compressed_size == idat.len() - 6, "tail mismatch");
                let recreated =
                    preflate_rs::recreate_whole_deflate_stream(text.text(), &data.corrections)?;
                ensure!(
                    recreated == idat[2..idat.len() - 4],
                    "bit-exact reconstruction failed"
                );
                corrections.add(&data.corrections)?;
                corrected += 1;
            }
            Err(_) => {
                failed += 1;
                corrections.add(&idat)?;
            }
        }
        if count % 50 == 0 {
            eprintln!(
                "{} real PNGs; {} exact deflate reconstructions; {:.1}s",
                count,
                corrected,
                start.elapsed().as_secs_f64()
            );
        }
    }
    for stream in [
        &mut filtered,
        &mut pixels,
        &mut original,
        &mut corrections,
        &mut aux,
    ] {
        stream.flush()?;
    }
    let report = serde_json::json!({"diagnostic":"Unique real PNG preprocessing; payload-only screening; references not included","images":count,"native_bytes":native,"inflated_bytes":inflated,"original_cdc_zstd_bytes":original.bytes,"filtered_cdc_zstd_bytes":filtered.bytes,"pixels_cdc_zstd_bytes":pixels.bytes,"reconstruction_bytes":corrections.bytes,"auxiliary_bytes":aux.bytes,"pixel_unique_bytes":pixels.raw,"pixel_chunks":pixels.chunks,"filtered_chunks":filtered.chunks,"exact_deflate_reconstructions":corrected,"fallbacks":failed,"seconds":start.elapsed().as_secs_f64()});
    fs::write(&args[2], serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    Ok(())
}
