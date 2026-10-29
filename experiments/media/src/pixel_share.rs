//! Exact scanline/tile sharing, with persisted recipes and independent readback.
//! This measures pixel storage only, excluding PNG reconstruction corrections.
use anyhow::{Result, ensure};
use nenia_media_screen::png;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};
#[derive(Deserialize)]
struct Asset {
    file: String,
    kind: String,
    cohort: String,
}
#[derive(Serialize, Deserialize)]
struct Picture {
    ordinal: usize,
    original: bool,
    width: usize,
    height: usize,
    channels: usize,
    hash: String,
    parts: Vec<usize>,
}
#[derive(Serialize, Deserialize)]
struct Pack {
    width: usize,
    height: usize,
    channels: usize,
    codec: u8,
    hash: String,
}
#[derive(Serialize, Deserialize, Clone, Copy)]
struct Chunk {
    pack: usize,
    offset: usize,
    len: usize,
}
#[derive(Serialize, Deserialize)]
struct Index {
    version: u32,
    tile: usize,
    pictures: Vec<Picture>,
    packs: Vec<Pack>,
    chunks: Vec<Chunk>,
}
struct Pending {
    raw: Vec<u8>,
    ids: Vec<usize>,
}
struct Store {
    root: PathBuf,
    index: Index,
    seen: HashMap<(usize, usize, usize, blake3::Hash), usize>,
    pending: HashMap<(usize, usize), Pending>,
    bytes: u64,
    unique_bytes: u64,
}
impl Store {
    fn flush(&mut self, key: (usize, usize)) -> Result<()> {
        let Some(pending) = self.pending.remove(&key) else {
            return Ok(());
        };
        if pending.raw.is_empty() {
            return Ok(());
        }
        let (width, channels) = key;
        let height = pending.raw.len() / (width * channels);
        let mut encoded = zstd::stream::encode_all(pending.raw.as_slice(), 9)?;
        let mut codec = 0;
        if matches!(channels, 3 | 4) {
            let candidate = png::webp_encode(&pending.raw, width, height, channels)?;
            if candidate.len() < encoded.len() {
                encoded = candidate;
                codec = 1;
            }
        }
        let hash = blake3::hash(&encoded).to_hex().to_string();
        let pack = self.index.packs.len();
        fs::write(self.root.join(format!("{pack:06}.pixels")), &encoded)?;
        self.bytes += encoded.len() as u64;
        self.index.packs.push(Pack {
            width,
            height,
            channels,
            codec,
            hash,
        });
        for id in pending.ids {
            self.index.chunks[id].pack = pack;
        }
        Ok(())
    }
    fn add(&mut self, bytes: &[u8], width: usize, height: usize, channels: usize) -> Result<usize> {
        let hash = blake3::hash(bytes);
        let key = (width, height, channels, hash);
        if let Some(&id) = self.seen.get(&key) {
            return Ok(id);
        }
        let group = (width, channels);
        if self.pending.get(&group).is_some_and(|p| {
            p.raw.len() + bytes.len() > 8 * 1024 * 1024
                || (p.raw.len() + bytes.len()) / (width * channels) > 16384
        }) {
            self.flush(group)?;
        }
        if self.pending.values().map(|p| p.raw.len()).sum::<usize>() > 64 * 1024 * 1024 {
            let largest = *self
                .pending
                .iter()
                .max_by_key(|(_, p)| p.raw.len())
                .unwrap()
                .0;
            self.flush(largest)?;
        }
        let pending = self.pending.entry(group).or_insert_with(|| Pending {
            raw: Vec::new(),
            ids: Vec::new(),
        });
        let id = self.index.chunks.len();
        self.index.chunks.push(Chunk {
            pack: usize::MAX,
            offset: pending.raw.len(),
            len: bytes.len(),
        });
        pending.raw.extend_from_slice(bytes);
        pending.ids.push(id);
        self.seen.insert(key, id);
        self.unique_bytes += bytes.len() as u64;
        Ok(id)
    }
    fn finish(&mut self) -> Result<u64> {
        let mut groups = self.pending.keys().copied().collect::<Vec<_>>();
        groups.sort();
        for key in groups {
            self.flush(key)?;
        }
        let encoded = zstd::stream::encode_all(serde_json::to_vec(&self.index)?.as_slice(), 9)?;
        fs::write(self.root.join("index.zstd"), &encoded)?;
        Ok(encoded.len() as u64)
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 5,
        "usage: pixel-share PRIVATE_ASSETS PRIVATE_OUTPUT REPORT TILE_SIZE_OR_ZERO_FOR_ROWS"
    );
    let input = Path::new(&args[1]);
    let output = Path::new(&args[2]);
    let tile: usize = args[4].parse()?;
    ensure!(tile == 0 || [16, 32, 64, 128].contains(&tile), "tile size");
    ensure!(!output.exists(), "output exists");
    fs::create_dir_all(output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(output, fs::Permissions::from_mode(0o700))?;
    }
    let assets: Vec<Asset> = serde_json::from_slice(&fs::read(input.join("assets.json"))?)?;
    let mut store = Store {
        root: output.into(),
        index: Index {
            version: 1,
            tile,
            pictures: Vec::new(),
            packs: Vec::new(),
            chunks: Vec::new(),
        },
        seen: HashMap::new(),
        pending: HashMap::new(),
        bytes: 0,
        unique_bytes: 0,
    };
    let start = Instant::now();
    let mut total_pixels = 0;
    let mut skipped = 0;
    let mut original_checkpoint = None;
    for (ordinal, asset) in assets.iter().enumerate() {
        if asset.cohort != "original" && original_checkpoint.is_none() {
            let index_bytes = store.finish()?;
            original_checkpoint = Some(
                serde_json::json!({"images":store.index.pictures.len(),"payload_bytes":store.bytes,"index_bytes":index_bytes,"container_bytes":store.bytes+index_bytes,"unique_pixel_bytes":store.unique_bytes,"chunks":store.index.chunks.len()}),
            );
        }
        if asset.kind != "png" {
            continue;
        }
        let img = match png::parse(&fs::read(input.join(&asset.file))?) {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if img.depth != 8 {
            skipped += 1;
            continue;
        }
        let mut parts = Vec::new();
        let row = img.width * img.channels;
        let th = if tile == 0 { 1 } else { tile };
        let tw = if tile == 0 { img.width } else { tile };
        for y in (0..img.height).step_by(th) {
            for x in (0..img.width).step_by(tw) {
                let w = tw.min(img.width - x);
                let h = th.min(img.height - y);
                let mut bytes = Vec::with_capacity(w * h * img.channels);
                for yy in y..y + h {
                    bytes.extend_from_slice(
                        &img.pixels[yy * row + x * img.channels..yy * row + (x + w) * img.channels],
                    );
                }
                parts.push(store.add(&bytes, w, h, img.channels)?);
            }
        }
        total_pixels += img.pixels.len() as u64;
        store.index.pictures.push(Picture {
            ordinal,
            original: asset.cohort == "original",
            width: img.width,
            height: img.height,
            channels: img.channels,
            hash: blake3::hash(&img.pixels).to_hex().to_string(),
            parts,
        });
        if store.index.pictures.len().is_multiple_of(100) {
            eprintln!(
                "{} pixel images; {} unique fragments; {:.1}s",
                store.index.pictures.len(),
                store.index.chunks.len(),
                start.elapsed().as_secs_f64()
            );
        }
    }
    let index_bytes = store.finish()?;
    let encode_seconds = start.elapsed().as_secs_f64();
    let payload_bytes = store.bytes;
    let unique_bytes = store.unique_bytes;
    drop(store);
    let start = Instant::now();
    let raw = zstd::stream::decode_all(fs::read(output.join("index.zstd"))?.as_slice())?;
    let index: Index = serde_json::from_slice(&raw)?;
    ensure!(index.version == 1 && index.tile == tile, "index identity");
    let mut cache = VecDeque::<(usize, Vec<u8>)>::new();
    let mut read_bytes = 0u64;
    for picture in &index.pictures {
        let mut pixels = vec![0; picture.width * picture.height * picture.channels];
        let row = picture.width * picture.channels;
        let th = if tile == 0 { 1 } else { tile };
        let tw = if tile == 0 { picture.width } else { tile };
        let mut part = 0;
        for y in (0..picture.height).step_by(th) {
            for x in (0..picture.width).step_by(tw) {
                let chunk = index.chunks[picture.parts[part]];
                part += 1;
                if let Some(position) = cache.iter().position(|(id, _)| *id == chunk.pack) {
                    let hit = cache.remove(position).unwrap();
                    cache.push_front(hit);
                } else {
                    let pack = &index.packs[chunk.pack];
                    let bytes = fs::read(output.join(format!("{:06}.pixels", chunk.pack)))?;
                    ensure!(
                        blake3::hash(&bytes).to_hex().as_str() == pack.hash,
                        "pack hash"
                    );
                    let raw = match pack.codec {
                        0 => zstd::stream::decode_all(bytes.as_slice())?,
                        1 => png::webp_decode(&bytes, pack.width, pack.height, pack.channels)?,
                        _ => anyhow::bail!("pack codec"),
                    };
                    ensure!(
                        raw.len() == pack.width * pack.height * pack.channels,
                        "pack dimensions"
                    );
                    read_bytes += raw.len() as u64;
                    if cache.len() == 8 {
                        cache.pop_back();
                    }
                    cache.push_front((chunk.pack, raw));
                }
                let bytes = &cache.front().unwrap().1[chunk.offset..chunk.offset + chunk.len];
                let w = tw.min(picture.width - x);
                let h = th.min(picture.height - y);
                ensure!(
                    bytes.len() == w * h * picture.channels,
                    "fragment dimensions"
                );
                for yy in 0..h {
                    pixels[(y + yy) * row + x * picture.channels
                        ..(y + yy) * row + (x + w) * picture.channels]
                        .copy_from_slice(
                            &bytes[yy * w * picture.channels..(yy + 1) * w * picture.channels],
                        );
                }
            }
        }
        let original = png::parse(&fs::read(input.join(&assets[picture.ordinal].file))?)?;
        ensure!(
            pixels == original.pixels && blake3::hash(&pixels).to_hex().as_str() == picture.hash,
            "pixel restore differs"
        );
    }
    let report = serde_json::json!({"candidate":"shared-pixel-fragments","scope":"Complete pixel-only archive; excludes PNG shell and DEFLATE correction streams; one exploratory trial","tile":tile,"original":original_checkpoint,"images":index.pictures.len(),"skipped_images":skipped,"pixels_bytes":total_pixels,"unique_pixels_bytes":unique_bytes,"chunks":index.chunks.len(),"packs":index.packs.len(),"payload_bytes":payload_bytes,"index_bytes":index_bytes,"container_bytes":payload_bytes+index_bytes,"encode_seconds":encode_seconds,"restore_with_oracle_seconds":start.elapsed().as_secs_f64(),"cache_decoded_bytes":read_bytes,"exact_pixel_restores":index.pictures.len()});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    Ok(())
}
