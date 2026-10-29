//! Self-contained pixel dependency recipes with content-addressed leaf objects.
//! Recipes embed their own pack tables, so restoring an image needs no catalog.
use crate::png;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
};
const LIMIT: usize = 128 * 1024 * 1024;
#[derive(Serialize, Deserialize, Clone)]
struct Packed {
    width: usize,
    height: usize,
    channels: usize,
    codec: u8,
    hash: String,
}
#[derive(Serialize, Deserialize, Clone)]
struct Part {
    pack: usize,
    offset: usize,
    len: usize,
}
#[derive(Deserialize)]
struct Picture {
    ordinal: usize,
    width: usize,
    height: usize,
    channels: usize,
    hash: String,
    parts: Vec<usize>,
}
#[derive(Deserialize)]
struct RowIndex {
    version: u32,
    tile: usize,
    pictures: Vec<Picture>,
    packs: Vec<Packed>,
    chunks: Vec<Part>,
}
#[derive(Deserialize)]
struct Frame {
    width: usize,
    height: usize,
    channels: usize,
    parent: Option<usize>,
    dy: i32,
    subtract: bool,
    codec: u8,
    hash: String,
}
#[derive(Serialize, Deserialize)]
enum Recipe {
    Fragments {
        width: usize,
        height: usize,
        channels: usize,
        hash: String,
        tile: usize,
        packs: Vec<Packed>,
        parts: Vec<Part>,
    },
    Delta {
        frame: Packed,
        base: Option<Packed>,
        dy: i32,
        subtract: bool,
        hash: String,
    },
}
pub struct Prepared {
    recipes: HashMap<usize, Vec<u8>>,
    objects: HashMap<String, (PathBuf, usize)>,
}
impl Prepared {
    pub fn open(root: &Path) -> Result<Self> {
        let mut this = Self {
            recipes: HashMap::new(),
            objects: HashMap::new(),
        };
        if root.join("index.zstd").exists() {
            let index: RowIndex = serde_json::from_slice(&inflate(
                &fs::read(root.join("index.zstd"))?,
                64 * 1024 * 1024,
            )?)?;
            ensure!(index.version == 1, "pixel index version");
            for (id, pack) in index.packs.iter().enumerate() {
                this.objects
                    .insert(pack.hash.clone(), (root.join(format!("{id:06}.pixels")), 0));
            }
            for picture in index.pictures {
                let mut ids = HashMap::new();
                let mut packs = Vec::new();
                let mut parts = Vec::new();
                for id in picture.parts {
                    let p = &index.chunks[id];
                    let local = *ids.entry(p.pack).or_insert_with(|| {
                        packs.push(index.packs[p.pack].clone());
                        packs.len() - 1
                    });
                    parts.push(Part {
                        pack: local,
                        offset: p.offset,
                        len: p.len,
                    });
                }
                let recipe = Recipe::Fragments {
                    width: picture.width,
                    height: picture.height,
                    channels: picture.channels,
                    hash: picture.hash,
                    tile: index.tile,
                    packs,
                    parts,
                };
                this.recipes.insert(
                    picture.ordinal,
                    zstd::stream::encode_all(serde_json::to_vec(&recipe)?.as_slice(), 9)?,
                );
            }
        } else {
            let mut frames = HashMap::new();
            for entry in fs::read_dir(root)? {
                let path = entry?.path();
                if path.extension().is_none_or(|s| s != "frame") {
                    continue;
                }
                let id: usize = path.file_stem().unwrap().to_str().unwrap().parse()?;
                let bytes = fs::read(&path)?;
                ensure!(
                    bytes.len() >= 12 && &bytes[..8] == b"NENIATM1",
                    "temporal header"
                );
                let n = u32::from_le_bytes(bytes[8..12].try_into()?) as usize;
                ensure!(n <= 1024 && 12 + n <= bytes.len(), "temporal header size");
                let frame: Frame = serde_json::from_slice(&bytes[12..12 + n])?;
                let hash = blake3::hash(&bytes[12 + n..]).to_hex().to_string();
                let packed = Packed {
                    width: frame.width,
                    height: frame.height,
                    channels: frame.channels,
                    codec: frame.codec,
                    hash: hash.clone(),
                };
                this.objects.insert(hash, (path, 12 + n));
                frames.insert(id, (frame, packed));
            }
            for (&id, (frame, packed)) in &frames {
                let base = frame
                    .parent
                    .map(|parent| -> Result<Packed> {
                        let (base, packed) = frames
                            .get(&parent)
                            .ok_or_else(|| anyhow::anyhow!("missing keyframe"))?;
                        ensure!(parent < id && base.parent.is_none(), "temporal graph depth");
                        Ok(packed.clone())
                    })
                    .transpose()?;
                let recipe = Recipe::Delta {
                    frame: packed.clone(),
                    base,
                    dy: frame.dy,
                    subtract: frame.subtract,
                    hash: frame.hash.clone(),
                };
                this.recipes.insert(
                    id,
                    zstd::stream::encode_all(serde_json::to_vec(&recipe)?.as_slice(), 9)?,
                );
            }
        }
        Ok(this)
    }
    pub fn materialize(&self, id: usize, out: &Path) -> Result<Option<Vec<u8>>> {
        let Some(token) = self.recipes.get(&id) else {
            return Ok(None);
        };
        let recipe: Recipe = serde_json::from_slice(&inflate(token, 4 * 1024 * 1024)?)?;
        let packs = match &recipe {
            Recipe::Fragments { packs, .. } => packs.iter().collect::<Vec<_>>(),
            Recipe::Delta { frame, base, .. } => {
                std::iter::once(frame).chain(base.iter()).collect()
            }
        };
        fs::create_dir_all(out)?;
        for pack in packs {
            let (path, offset) = &self.objects[&pack.hash];
            let target = out.join(&pack.hash);
            if target.exists() {
                continue;
            }
            let bytes = fs::read(path)?;
            let bytes = &bytes[*offset..];
            ensure!(
                blake3::hash(bytes).to_hex().as_str() == pack.hash,
                "prepared pixel object changed"
            );
            crate::pack::atomic_write_immutable(&target, bytes)?;
        }
        Ok(Some(token.clone()))
    }
}
fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut d = zstd::stream::read::Decoder::new(data)?;
    d.window_log_max(27)?;
    let mut out = Vec::new();
    d.take(limit as u64 + 1).read_to_end(&mut out)?;
    ensure!(out.len() <= limit, "pixel decompression limit");
    Ok(out)
}
pub struct Reader {
    root: PathBuf,
    cache: VecDeque<(String, Vec<u8>)>,
}
impl Reader {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: VecDeque::new(),
        }
    }
    fn packed(&mut self, pack: &Packed) -> Result<&[u8]> {
        ensure!(
            pack.width > 0
                && pack.height > 0
                && pack.width <= 16384
                && pack.height <= 16384
                && (1..=4).contains(&pack.channels)
                && pack.width * pack.height * pack.channels <= LIMIT,
            "pixel object layout"
        );
        if let Some(pos) = self.cache.iter().position(|(hash, _)| hash == &pack.hash) {
            let hit = self.cache.remove(pos).unwrap();
            self.cache.push_front(hit);
        } else {
            ensure!(
                pack.hash.len() == 64
                    && pack
                        .hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "pixel object name"
            );
            let encoded = fs::read(self.root.join(&pack.hash))?;
            ensure!(
                blake3::hash(&encoded).to_hex().as_str() == pack.hash,
                "pixel object hash"
            );
            let raw = match pack.codec {
                0 => inflate(&encoded, LIMIT)?,
                1 => png::webp_decode(&encoded, pack.width, pack.height, pack.channels)?,
                _ => anyhow::bail!("pixel object codec"),
            };
            ensure!(
                raw.len() == pack.width * pack.height * pack.channels,
                "pixel object size"
            );
            while !self.cache.is_empty()
                && self.cache.iter().map(|(_, v)| v.len()).sum::<usize>() + raw.len() > LIMIT
            {
                self.cache.pop_back();
            }
            self.cache.push_front((pack.hash.clone(), raw));
        }
        let raw = &self.cache.front().unwrap().1;
        ensure!(
            raw.len() == pack.width * pack.height * pack.channels,
            "cached pixel object layout"
        );
        Ok(raw)
    }
    pub fn resolve(&mut self, token: &[u8]) -> Result<Vec<u8>> {
        let recipe: Recipe = serde_json::from_slice(&inflate(token, 4 * 1024 * 1024)?)?;
        let (pixels, hash) = match recipe {
            Recipe::Fragments {
                width,
                height,
                channels,
                hash,
                tile,
                packs,
                parts,
            } => {
                ensure!(
                    width > 0
                        && height > 0
                        && width <= 16384
                        && height <= 16384
                        && (1..=4).contains(&channels)
                        && width * height * channels <= LIMIT
                        && (tile == 0 || [16, 32, 64, 128].contains(&tile)),
                    "fragment layout"
                );
                let th = if tile == 0 { 1 } else { tile };
                let tw = if tile == 0 { width } else { tile };
                ensure!(
                    parts.len() == height.div_ceil(th) * width.div_ceil(tw),
                    "fragment count"
                );
                let mut pixels = vec![0; width * height * channels];
                let mut id = 0;
                for y in (0..height).step_by(th) {
                    for x in (0..width).step_by(tw) {
                        let p = &parts[id];
                        id += 1;
                        ensure!(p.pack < packs.len(), "fragment pack index");
                        let bytes = self.packed(&packs[p.pack])?;
                        ensure!(
                            p.offset <= bytes.len() && p.len <= bytes.len() - p.offset,
                            "fragment bounds"
                        );
                        let bytes = &bytes[p.offset..p.offset + p.len];
                        let w = tw.min(width - x);
                        let h = th.min(height - y);
                        ensure!(bytes.len() == w * h * channels, "fragment dimensions");
                        for yy in 0..h {
                            pixels[((y + yy) * width + x) * channels
                                ..((y + yy) * width + x + w) * channels]
                                .copy_from_slice(
                                    &bytes[yy * w * channels..(yy + 1) * w * channels],
                                );
                        }
                    }
                }
                (pixels, hash)
            }
            Recipe::Delta {
                frame,
                base,
                dy,
                subtract,
                hash,
            } => {
                let mut pixels = self.packed(&frame)?.to_vec();
                if let Some(base) = base {
                    ensure!(
                        base.width == frame.width
                            && base.channels == frame.channels
                            && dy.abs() <= 16384,
                        "delta layout"
                    );
                    let data = self.packed(&base)?;
                    let row = frame.width * frame.channels;
                    for y in 0..frame.height {
                        let source = y as i64 + dy as i64;
                        if source >= 0 && (source as usize) < base.height {
                            for x in 0..row {
                                let v = data[source as usize * row + x];
                                pixels[y * row + x] = if subtract {
                                    pixels[y * row + x].wrapping_add(v)
                                } else {
                                    pixels[y * row + x] ^ v
                                };
                            }
                        }
                    }
                }
                (pixels, hash)
            }
        };
        ensure!(
            blake3::hash(&pixels).to_hex().as_str() == hash,
            "reconstructed pixel hash"
        );
        Ok(pixels)
    }
}
