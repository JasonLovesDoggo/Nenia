//! Format-2 packs reconstruct original image bytes. Dependencies are exclusively
//! ordinary format-1 packs: no recursive image graph or external runtime service.
use crate::{pack, png, shared_pixels};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeSet,
    io::{Cursor, Write},
    path::PathBuf,
};
const MAGIC: &[u8; 8] = b"NENIAPK\0";
pub struct Header<'a> {
    pub codec: u16,
    pub raw_len: usize,
    pub dependencies: Vec<String>,
    pub payload: &'a [u8],
}
pub fn is_media(data: &[u8]) -> bool {
    data.len() >= 10 && &data[..8] == MAGIC && data[8..10] == 2u16.to_le_bytes()
}
pub fn header(data: &[u8]) -> Result<Header<'_>> {
    ensure!(
        is_media(data) && data.len() >= 24 && data.len() <= pack::MAX_PACK_ENCODED_SIZE,
        "media pack header/size"
    );
    let codec = u16::from_le_bytes(data[10..12].try_into()?);
    let raw_len = u64::from_le_bytes(data[12..20].try_into()?);
    ensure!(
        raw_len > 0 && raw_len <= pack::MAX_PACK_RAW_SIZE as u64,
        "media raw length"
    );
    let raw_len = raw_len as usize;
    let n = u32::from_le_bytes(data[20..24].try_into()?) as usize;
    ensure!(
        matches!(codec, 1..=3)
            && raw_len > 0
            && raw_len <= pack::MAX_PACK_RAW_SIZE
            && n <= 1024
            && 24 + n * 32 < data.len(),
        "media pack bounds"
    );
    ensure!(codec != 2 || n == 0, "JPEG pack cannot have dependencies");
    let mut dependencies = Vec::new();
    for bytes in data[24..24 + n * 32].chunks_exact(32) {
        dependencies.push(
            blake3::Hash::from_bytes(bytes.try_into()?)
                .to_hex()
                .to_string(),
        );
    }
    ensure!(
        dependencies.windows(2).all(|p| p[0] < p[1]),
        "media dependencies must be sorted and unique"
    );
    let payload = &data[24 + n * 32..];
    ensure!(
        payload.len() >= 45
            && &payload[..8]
                == match codec {
                    1 => b"NENIAPG2",
                    2 => b"NENIAJP2",
                    _ => b"NENIARF1",
                },
        "media payload version"
    );
    ensure!(
        u32::from_le_bytes(payload[9..13].try_into()?) as usize == raw_len,
        "media length disagreement"
    );
    Ok(Header {
        codec,
        raw_len,
        dependencies,
        payload,
    })
}
/// Validate transport independently of dependency upload order. Publication of a
/// manifest still requires complete reconstruction and its original-file hash.
pub fn preflight(data: &[u8], id: &str) -> Result<()> {
    pack::validate_hash_id(id)?;
    if !is_media(data) {
        pack::decode_pack(data, id)?;
        return Ok(());
    }
    ensure!(
        blake3::hash(data).to_hex().as_str() == id,
        "media pack hash mismatch"
    );
    header(data)?;
    Ok(())
}
pub fn encode(
    codec: u16,
    original: &[u8],
    payload: &[u8],
    dependencies: impl IntoIterator<Item = String>,
) -> Result<pack::EncodedPack> {
    let dependencies = dependencies.into_iter().collect::<BTreeSet<_>>();
    let mut bytes = MAGIC.to_vec();
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(codec.to_le_bytes());
    bytes.extend((original.len() as u64).to_le_bytes());
    bytes.extend((dependencies.len() as u32).to_le_bytes());
    for id in dependencies {
        pack::validate_hash_id(&id)?;
        bytes.extend(blake3::Hash::from_hex(&id)?.as_bytes());
    }
    bytes.extend(payload);
    let id = blake3::hash(&bytes).to_hex().to_string();
    preflight(&bytes, &id)?;
    Ok(pack::EncodedPack {
        id,
        bytes,
        raw_len: original.len() as u64,
    })
}
pub struct Decoder {
    pixels: shared_pixels::Reader,
}
impl Decoder {
    pub fn new(root: PathBuf) -> Self {
        Self {
            pixels: shared_pixels::Reader::archive(root),
        }
    }
    pub fn decode(&mut self, data: &[u8], id: &str) -> Result<Vec<u8>> {
        preflight(data, id)?;
        let h = header(data)?;
        self.pixels.begin_dependencies(&h.dependencies);
        let (codec, payload) = if h.codec == 3 {
            ensure!(
                h.payload.len() >= 49 && matches!(h.payload[8], 1 | 2),
                "referenced image header"
            );
            let n = u32::from_le_bytes(h.payload[45..49].try_into()?) as usize;
            ensure!(n > 0 && n <= 32 * 1024 * 1024, "image body budget");
            let hash = blake3::Hash::from_bytes(h.payload[13..45].try_into()?)
                .to_hex()
                .to_string();
            let mut decoder = zstd::stream::read::Decoder::new(&h.payload[49..])?;
            decoder.window_log_max(23)?;
            let mut raw = Vec::new();
            use std::io::Read;
            decoder.take(65537).read_to_end(&mut raw)?;
            ensure!(raw.len() <= 65536, "image body reference metadata limit");
            let parts: Vec<pack::ChunkRef> = serde_json::from_slice(&raw)?;
            let body = self.pixels.blob(&hash, &parts, n)?;
            ensure!(body.len() == n, "image body size");
            (h.payload[8] as u16, body)
        } else {
            (h.codec, h.payload.to_vec())
        };
        ensure!(
            payload.len() >= 45
                && u32::from_le_bytes(payload[9..13].try_into()?) as usize == h.raw_len
                && &payload[..8] == if codec == 1 { b"NENIAPG2" } else { b"NENIAJP2" },
            "inner image identity/format"
        );
        let raw = if codec == 1 {
            png::decode_with_pixels(&payload, |token| self.pixels.resolve(token))?
        } else {
            decode_jpeg(&payload)?
        };
        self.pixels.finish_dependencies()?;
        ensure!(raw.len() == h.raw_len, "media decoded size");
        Ok(raw)
    }
}
pub fn referenced(
    codec: u16,
    raw_len: usize,
    payload_hash: &str,
    payload_len: usize,
    parts: &[pack::ChunkRef],
    dependencies: &[String],
) -> Result<pack::EncodedPack> {
    ensure!(
        matches!(codec, 1 | 2)
            && raw_len <= pack::MAX_PACK_RAW_SIZE
            && payload_len > 0
            && payload_len <= 32 * 1024 * 1024
            && !parts.is_empty()
            && parts.len() <= 128,
        "referenced image budget"
    );
    let mut body = b"NENIARF1".to_vec();
    body.push(codec as u8);
    body.extend((raw_len as u32).to_le_bytes());
    body.extend(blake3::Hash::from_hex(payload_hash)?.as_bytes());
    body.extend((payload_len as u32).to_le_bytes());
    body.extend(zstd::stream::encode_all(
        serde_json::to_vec(parts)?.as_slice(),
        9,
    )?);
    let deps = dependencies
        .iter()
        .cloned()
        .chain(parts.iter().map(|p| p.pack.clone()))
        .collect::<BTreeSet<_>>();
    // encode() uses only the original length; the inner body carries its hash.
    encode(3, &vec![0; raw_len], &body, deps)
}
struct Limited {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Limited {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.limit - self.bytes.len() {
            return Err(std::io::Error::other("JPEG output budget"));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub fn decode_jpeg(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        data.len() >= 53 && data.len() <= pack::MAX_PACK_ENCODED_SIZE && &data[..8] == b"NENIAJP2",
        "JPEG archive header"
    );
    let n = u32::from_le_bytes(data[9..13].try_into()?) as usize;
    ensure!(n <= pack::MAX_PACK_RAW_SIZE, "JPEG output size");
    let mut out = Limited {
        bytes: Vec::new(),
        limit: n,
    };
    match data[8] {
        0 => {
            let mut d = zstd::stream::read::Decoder::new(&data[53..])?;
            d.window_log_max(23)?;
            std::io::copy(&mut d, &mut out)?;
        }
        1 => {
            let width = u32::from_le_bytes(data[45..49].try_into()?);
            let height = u32::from_le_bytes(data[49..53].try_into()?);
            ensure!(
                width > 0
                    && height > 0
                    && width <= 8192
                    && height <= 8192
                    && width as u64 * height as u64 <= 16 * 1024 * 1024,
                "JPEG pixel budget"
            );
            let features = lepton_jpeg::EnabledFeatures {
                max_threads: 1,
                max_jpeg_width: width,
                max_jpeg_height: height,
                max_jpeg_file_size: n as u32,
                ..lepton_jpeg::EnabledFeatures::compat_lepton_vector_write()
            };
            lepton_jpeg::decode_lepton(
                &mut Cursor::new(&data[53..]),
                &mut out,
                &features,
                &lepton_jpeg::SingleThreadPool::default(),
            )?;
        }
        _ => anyhow::bail!("unknown JPEG archive codec"),
    }
    ensure!(
        out.bytes.len() == n && blake3::hash(&out.bytes).as_bytes() == &data[13..45],
        "JPEG identity mismatch"
    );
    Ok(out.bytes)
}
pub fn encode_jpeg(original: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        original.len() <= pack::MAX_PACK_RAW_SIZE,
        "JPEG source budget"
    );
    let mut payload = zstd::stream::encode_all(original, 9)?;
    let mut codec = 0;
    let mut dimensions = (0, 0);
    if let Some((width, height)) = jpeg_dimensions(original)
        && width <= 8192
        && height <= 8192
        && width as u64 * height as u64 <= 16 * 1024 * 1024
    {
        let features = lepton_jpeg::EnabledFeatures {
            max_threads: 1,
            max_jpeg_width: width,
            max_jpeg_height: height,
            max_jpeg_file_size: pack::MAX_PACK_RAW_SIZE as u32,
            ..lepton_jpeg::EnabledFeatures::compat_lepton_vector_write()
        };
        if let Ok((candidate, _)) = lepton_jpeg::encode_lepton_verify(
            original,
            &features,
            &lepton_jpeg::SingleThreadPool::default(),
        ) && candidate.len() < payload.len()
        {
            payload = candidate;
            codec = 1;
            dimensions = (width, height);
        }
    }
    let mut out = b"NENIAJP2".to_vec();
    out.push(codec);
    out.extend((original.len() as u32).to_le_bytes());
    out.extend(blake3::hash(original).as_bytes());
    out.extend(dimensions.0.to_le_bytes());
    out.extend(dimensions.1.to_le_bytes());
    out.extend(payload);
    ensure!(decode_jpeg(&out)? == original, "JPEG encoder proof");
    Ok(out)
}
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if !data.starts_with(&[255, 216]) {
        return None;
    }
    let mut pos = 2;
    while pos + 4 <= data.len() {
        if data[pos] != 255 {
            return None;
        }
        while data.get(pos) == Some(&255) {
            pos += 1;
        }
        let marker = *data.get(pos)?;
        pos += 1;
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        let n = u16::from_be_bytes(data.get(pos..pos + 2)?.try_into().ok()?) as usize;
        if n < 2 || pos + n > data.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc2) {
            let h = data.get(pos + 2..pos + 8)?;
            return Some((
                u16::from_be_bytes([h[3], h[4]]) as u32,
                u16::from_be_bytes([h[1], h[2]]) as u32,
            ));
        }
        pos += n;
    }
    None
}
