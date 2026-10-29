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
const LIMIT: usize = 32 * 1024 * 1024;
#[derive(Serialize, Deserialize, Clone)]
pub struct Packed {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub codec: u8,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage: Vec<crate::pack::ChunkRef>,
}
#[derive(Serialize, Deserialize, Clone)]
pub struct Part {
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
pub enum Recipe {
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
    Sprites {
        width: usize,
        height: usize,
        channels: usize,
        hash: String,
        tile: usize,
        background: Vec<u32>,
        packs: Vec<Packed>,
        placements: Vec<[u32; 4]>,
        #[serde(default)]
        base: Option<usize>,
    },
}
impl Recipe {
    fn leaves(&self) -> Vec<&Packed> {
        match self {
            Self::Fragments { packs, .. } | Self::Sprites { packs, .. } => packs.iter().collect(),
            Self::Delta { frame, base, .. } => std::iter::once(frame).chain(base.iter()).collect(),
        }
    }
    fn leaves_mut(&mut self) -> Vec<&mut Packed> {
        match self {
            Self::Fragments { packs, .. } | Self::Sprites { packs, .. } => {
                packs.iter_mut().collect()
            }
            Self::Delta { frame, base, .. } => {
                std::iter::once(frame).chain(base.iter_mut()).collect()
            }
        }
    }
}
pub struct Prepared {
    recipes: HashMap<usize, Vec<u8>>,
    objects: HashMap<String, (PathBuf, usize)>,
}
pub type Quote = (Vec<u8>, Vec<(String, u64)>);
impl Prepared {
    /// Prove equal decoded pixel arrays before unifying their encodings. This
    /// also rewrites delta bases: choosing a smaller keyframe never requires a
    /// second copy merely because its original dependent used another codec.
    pub fn canonicalize(sources: &mut [Self]) -> Result<serde_json::Value> {
        let mut locations = HashMap::new();
        for source in sources.iter() {
            locations.extend(source.objects.clone());
        }
        let mut reader = Reader::new(PathBuf::new());
        reader.locations = Some(locations.clone());
        let mut aliases = HashMap::<PixelKey, Packed>::new();
        let mut identities = HashMap::<(blake3::Hash, usize, usize, usize), (u64, Packed)>::new();
        let mut keys = HashMap::new();
        for source in sources.iter() {
            for token in source.recipes.values() {
                let recipe: Recipe = serde_json::from_slice(&inflate(token, 4 * 1024 * 1024)?)?;
                for pack in recipe.leaves() {
                    let encoded = (
                        pack.hash.clone(),
                        pack.width,
                        pack.height,
                        pack.channels,
                        pack.codec,
                    );
                    if keys.contains_key(&encoded) {
                        continue;
                    }
                    let hash = blake3::hash(reader.packed(pack)?);
                    let identity = (hash, pack.width, pack.height, pack.channels);
                    let (path, offset) = &locations[&pack.hash];
                    let bytes = fs::metadata(path)?.len() - *offset as u64;
                    let winner = identities.entry(identity).or_insert((bytes, pack.clone()));
                    if (bytes, &pack.hash) < (winner.0, &winner.1.hash) {
                        *winner = (bytes, pack.clone());
                    }
                    keys.insert(encoded, identity);
                }
            }
        }
        let mut replaced = 0;
        let mut possible_savings = 0;
        for (encoded, identity) in keys {
            let (cost, winner) = &identities[&identity];
            if encoded.0 != winner.hash {
                replaced += 1;
                let (path, offset) = &locations[&encoded.0];
                possible_savings += fs::metadata(path)?.len() - *offset as u64 - *cost;
            }
            aliases.insert(encoded, winner.clone());
        }
        for source in sources {
            source.objects.extend(locations.clone());
            for token in source.recipes.values_mut() {
                let mut recipe: Recipe = serde_json::from_slice(&inflate(token, 4 * 1024 * 1024)?)?;
                for p in recipe.leaves_mut() {
                    *p = aliases[&(p.hash.clone(), p.width, p.height, p.channels, p.codec)].clone();
                }
                *token = zstd::stream::encode_all(serde_json::to_vec(&recipe)?.as_slice(), 9)?;
            }
        }
        Ok(
            serde_json::json!({"unique_pixel_arrays":identities.len(),"replaced_encodings":replaced,"potential_bytes_before_selection":possible_savings,"identity_proof":"BLAKE3 of exact decoded pixels plus dimensions and channels; complete selected images are separately byte-compared"}),
        )
    }
    pub fn quote(&self, id: usize) -> Result<Option<Quote>> {
        let Some(token) = self.recipes.get(&id) else {
            return Ok(None);
        };
        let recipe: Recipe = serde_json::from_slice(&inflate(token, 4 * 1024 * 1024)?)?;
        let packs = match &recipe {
            Recipe::Fragments { packs, .. } | Recipe::Sprites { packs, .. } => {
                packs.iter().collect::<Vec<_>>()
            }
            Recipe::Delta { frame, base, .. } => {
                std::iter::once(frame).chain(base.iter()).collect()
            }
        };
        let mut objects = std::collections::BTreeMap::new();
        for p in packs {
            crate::pack::validate_hash_id(&p.hash)?;
            let (path, offset) = &self.objects[&p.hash];
            let size = fs::metadata(path)?.len();
            ensure!(*offset as u64 <= size, "pixel object offset");
            objects.insert(p.hash.clone(), size - *offset as u64);
        }
        Ok(Some((token.clone(), objects.into_iter().collect())))
    }
    pub fn open(root: &Path) -> Result<Self> {
        let mut this = Self {
            recipes: HashMap::new(),
            objects: HashMap::new(),
        };
        if root.join("recipes").exists() {
            for entry in fs::read_dir(root.join("recipes"))? {
                let path = entry?.path();
                if path.extension().is_none_or(|s| s != "pixel") {
                    continue;
                }
                let id = path.file_stem().unwrap().to_str().unwrap().parse()?;
                let token = fs::read(path)?;
                let recipe: Recipe = serde_json::from_slice(&inflate(&token, 4 * 1024 * 1024)?)?;
                let packs = match &recipe {
                    Recipe::Fragments { packs, .. } | Recipe::Sprites { packs, .. } => {
                        packs.iter().collect::<Vec<_>>()
                    }
                    Recipe::Delta { frame, base, .. } => {
                        std::iter::once(frame).chain(base.iter()).collect()
                    }
                };
                for pack in packs {
                    this.objects.insert(
                        pack.hash.clone(),
                        (root.join("objects").join(&pack.hash), 0),
                    );
                }
                this.recipes.insert(id, token);
            }
        } else if root.join("index.zstd").exists() {
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
                    storage: Vec::new(),
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
            Recipe::Fragments { packs, .. } | Recipe::Sprites { packs, .. } => {
                packs.iter().collect::<Vec<_>>()
            }
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
type PixelKey = (String, usize, usize, usize, u8);
type CacheKey = (PixelKey, blake3::Hash);
pub struct Reader {
    root: PathBuf,
    cache: VecDeque<(CacheKey, Vec<u8>)>,
    small: HashMap<CacheKey, Vec<u8>>,
    locations: Option<HashMap<String, (PathBuf, usize)>>,
    archive: bool,
    allowed: Option<std::collections::HashSet<String>>,
    used: std::collections::HashSet<String>,
    leaf_cache: VecDeque<(String, Vec<u8>)>,
}
impl Reader {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: VecDeque::new(),
            small: HashMap::new(),
            locations: None,
            archive: false,
            allowed: None,
            used: std::collections::HashSet::new(),
            leaf_cache: VecDeque::new(),
        }
    }
    pub fn archive(root: PathBuf) -> Self {
        let mut reader = Self::new(root);
        reader.archive = true;
        reader
    }
    pub fn begin_dependencies(&mut self, ids: &[String]) {
        self.allowed = Some(ids.iter().cloned().collect());
        self.used.clear();
    }
    pub fn finish_dependencies(&self) -> Result<()> {
        ensure!(
            self.allowed.as_ref().is_none_or(|a| a == &self.used),
            "media dependency list mismatch"
        );
        Ok(())
    }
    pub fn blob(
        &mut self,
        hash: &str,
        parts: &[crate::pack::ChunkRef],
        limit: usize,
    ) -> Result<Vec<u8>> {
        crate::pack::validate_hash_id(hash)?;
        ensure!(
            !parts.is_empty() && parts.len() <= 128 && limit <= 32 * 1024 * 1024,
            "blob reference budget"
        );
        let mut out = Vec::new();
        for part in parts {
            crate::pack::validate_hash_id(&part.pack)?;
            ensure!(
                part.len > 0 && part.len as usize <= crate::pack::MAX_CHUNK_SIZE,
                "blob chunk size"
            );
            ensure!(
                self.allowed.as_ref().is_none_or(|a| a.contains(&part.pack)),
                "undeclared blob dependency"
            );
            self.used.insert(part.pack.clone());
            if let Some(i) = self.leaf_cache.iter().position(|(id, _)| id == &part.pack) {
                let p = self.leaf_cache.remove(i).unwrap();
                self.leaf_cache.push_front(p);
            } else {
                let bytes = read_bounded(
                    &self.root.join(format!("{}.nz", part.pack)),
                    crate::pack::MAX_PACK_ENCODED_SIZE,
                )?;
                let raw = crate::pack::decode_pack(&bytes, &part.pack)?;
                if self.leaf_cache.len() == 8 {
                    self.leaf_cache.pop_back();
                }
                self.leaf_cache.push_front((part.pack.clone(), raw));
            }
            let bytes = crate::pack::extract_chunk(&self.leaf_cache.front().unwrap().1, part)?;
            ensure!(bytes.len() <= limit - out.len(), "blob decoded size");
            out.extend(bytes);
        }
        ensure!(
            blake3::hash(&out).to_hex().as_str() == hash,
            "blob identity mismatch"
        );
        Ok(out)
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
        let mut storage_hash = blake3::Hasher::new();
        for part in &pack.storage {
            crate::pack::validate_hash_id(&part.pack)?;
            if !part.hash.is_empty() {
                crate::pack::validate_hash_id(&part.hash)?;
            }
            ensure!(
                part.len > 0
                    && part.len as usize <= crate::pack::MAX_CHUNK_SIZE
                    && part
                        .offset
                        .checked_add(part.len as u64)
                        .is_some_and(|n| n <= crate::pack::MAX_PACK_RAW_SIZE as u64),
                "pixel storage bounds"
            );
            ensure!(
                self.allowed.as_ref().is_none_or(|a| a.contains(&part.pack)),
                "undeclared media dependency"
            );
            self.used.insert(part.pack.clone());
            storage_hash.update(part.pack.as_bytes());
            storage_hash.update(&part.offset.to_le_bytes());
            storage_hash.update(&part.len.to_le_bytes());
            storage_hash.update(part.hash.as_bytes());
        }
        ensure!(
            !self.archive || !pack.storage.is_empty(),
            "archive pixel storage required"
        );
        let key = (
            (
                pack.hash.clone(),
                pack.width,
                pack.height,
                pack.channels,
                pack.codec,
            ),
            storage_hash.finalize(),
        );
        if self.small.contains_key(&key) {
            let bytes = &self.small[&key];
            ensure!(
                bytes.len() == pack.width * pack.height * pack.channels,
                "cached sprite layout"
            );
            return Ok(bytes);
        }
        if let Some(pos) = self.cache.iter().position(|(stored, _)| stored == &key) {
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
            let encoded = if !pack.storage.is_empty() {
                ensure!(pack.storage.len() <= 128, "pixel storage reference limit");
                let mut encoded = Vec::new();
                for part in &pack.storage {
                    crate::pack::validate_hash_id(&part.pack)?;
                    if let Some(i) = self.leaf_cache.iter().position(|(id, _)| id == &part.pack) {
                        let p = self.leaf_cache.remove(i).unwrap();
                        self.leaf_cache.push_front(p);
                    } else {
                        let file = self.root.join(format!("{}.nz", part.pack));
                        let bytes = read_bounded(&file, crate::pack::MAX_PACK_ENCODED_SIZE)?;
                        // Strict format-1 decoding rejects recursive media dependencies.
                        let raw = crate::pack::decode_pack(&bytes, &part.pack)?;
                        if self.leaf_cache.len() == 8 {
                            self.leaf_cache.pop_back();
                        }
                        self.leaf_cache.push_front((part.pack.clone(), raw));
                    }
                    let bytes =
                        crate::pack::extract_chunk(&self.leaf_cache.front().unwrap().1, part)?;
                    ensure!(
                        encoded.len() + bytes.len() <= 16 * 1024 * 1024,
                        "encoded pixel object budget"
                    );
                    encoded.extend_from_slice(bytes);
                }
                encoded
            } else if let Some(locations) = &self.locations {
                let (path, offset) = locations
                    .get(&pack.hash)
                    .ok_or_else(|| anyhow::anyhow!("missing pixel object"))?;
                let bytes = fs::read(path)?;
                ensure!(*offset <= bytes.len(), "pixel object prefix");
                bytes[*offset..].to_vec()
            } else {
                ensure!(
                    !self.archive,
                    "archive pixels require immutable pack references"
                );
                read_bounded(&self.root.join(&pack.hash), 16 * 1024 * 1024)?
            };
            ensure!(
                blake3::hash(&encoded).to_hex().as_str() == pack.hash,
                "pixel object hash"
            );
            let raw = match pack.codec {
                0 => inflate(&encoded, LIMIT)?,
                1 => png::webp_decode(&encoded, pack.width, pack.height, pack.channels)?,
                2 => png::jxl_decode(&encoded, pack.width, pack.height, pack.channels)?,
                _ => anyhow::bail!("pixel object codec"),
            };
            ensure!(
                raw.len() == pack.width * pack.height * pack.channels,
                "pixel object size"
            );
            if raw.len() <= 4096 {
                if self.small.len() == 4096 {
                    self.small.clear();
                }
                self.small.insert(key.clone(), raw);
                return Ok(&self.small[&key]);
            }
            while !self.cache.is_empty()
                && self.cache.iter().map(|(_, v)| v.len()).sum::<usize>() + raw.len() > LIMIT
            {
                self.cache.pop_back();
            }
            self.cache.push_front((key, raw));
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
            Recipe::Sprites {
                width,
                height,
                channels,
                hash,
                tile,
                background,
                packs,
                placements,
                base,
            } => {
                ensure!(
                    width > 0
                        && height > 0
                        && width <= 16384
                        && height <= 16384
                        && (1..=4).contains(&channels)
                        && width * height * channels <= LIMIT
                        && tile == 32,
                    "sprite image layout"
                );
                let cols = width.div_ceil(tile);
                ensure!(
                    ((base.is_none() && background.len() == cols * height.div_ceil(tile))
                        || (base.is_some() && background.is_empty()))
                        && placements.len() <= 100_000,
                    "sprite recipe bounds"
                );
                let mut pixels = if let Some(id) = base {
                    ensure!(
                        id < packs.len()
                            && packs[id].width == width
                            && packs[id].height == height
                            && packs[id].channels == channels,
                        "sprite base layout"
                    );
                    self.packed(&packs[id])?.to_vec()
                } else {
                    vec![0; width * height * channels]
                };
                if base.is_none() {
                    for y in 0..height {
                        for x in 0..width {
                            let color = background[y / tile * cols + x / tile].to_le_bytes();
                            pixels[(y * width + x) * channels..(y * width + x + 1) * channels]
                                .copy_from_slice(&color[..channels]);
                        }
                    }
                }
                let mut work = 0;
                for [id, x, y, invert] in placements {
                    let (id, x, y) = (id as usize, x as usize, y as usize);
                    ensure!(id < packs.len() && invert <= 1, "sprite placement");
                    let p = &packs[id];
                    ensure!(
                        x <= width
                            && y <= height
                            && p.width <= width - x
                            && p.height <= height - y
                            && (p.channels == channels || (p.channels == 3 && channels == 4)),
                        "sprite geometry"
                    );
                    work += p.width * p.height * channels;
                    ensure!(work <= pixels.len() * 4, "sprite work budget");
                    let patch = self.packed(p)?;
                    for yy in 0..p.height {
                        for xx in 0..p.width {
                            for c in 0..p.channels {
                                let v = patch[(yy * p.width + xx) * p.channels + c];
                                let out = &mut pixels[((y + yy) * width + x + xx) * channels + c];
                                *out = if invert == 0 {
                                    out.wrapping_add(v)
                                } else {
                                    out.wrapping_sub(v)
                                };
                            }
                        }
                    }
                }
                (pixels, hash)
            }
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
                            && dy.unsigned_abs() <= 16384,
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

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut out)?;
    ensure!(out.len() <= limit, "pixel object exceeds encoded budget");
    Ok(out)
}
/// Bind a pixel recipe to ordinary immutable packs. Returns its direct pack IDs.
pub fn bind_storage(
    token: &[u8],
    storage: &HashMap<String, Vec<crate::pack::ChunkRef>>,
) -> Result<(Vec<u8>, Vec<String>)> {
    let mut recipe: Recipe = serde_json::from_slice(&inflate(token, 4 * 1024 * 1024)?)?;
    let mut ids = std::collections::BTreeSet::new();
    for leaf in recipe.leaves_mut() {
        leaf.storage = storage
            .get(&leaf.hash)
            .ok_or_else(|| anyhow::anyhow!("missing packed pixel object"))?
            .clone();
        ensure!(
            !leaf.storage.is_empty() && leaf.storage.len() <= 128,
            "pixel storage references"
        );
        for part in &leaf.storage {
            crate::pack::validate_hash_id(&part.pack)?;
            ids.insert(part.pack.clone());
        }
    }
    Ok((
        zstd::stream::encode_all(serde_json::to_vec(&recipe)?.as_slice(), 9)?,
        ids.into_iter().collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cached_pixels_do_not_bypass_layout_or_storage_validation() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let rgba = [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];
        let encoded = png::webp_encode(&rgba, 2, 2, 4)?;
        let hash = blake3::hash(&encoded).to_hex().to_string();
        fs::write(dir.path().join(&hash), encoded)?;
        let mut p = Packed {
            width: 2,
            height: 2,
            channels: 3,
            codec: 1,
            hash,
            storage: Vec::new(),
        };
        let mut reader = Reader::new(dir.path().into());
        assert_eq!(reader.packed(&p)?.len(), 12);
        p.channels = 4;
        assert_eq!(reader.packed(&p)?, rgba);
        p.width = 4;
        p.height = 1;
        assert!(reader.packed(&p).is_err());
        p.width = 2;
        p.height = 2;
        p.codec = 99;
        assert!(reader.packed(&p).is_err());
        p.codec = 1;
        p.storage = vec![crate::pack::ChunkRef {
            hash: String::new(),
            pack: "invalid".into(),
            offset: 0,
            len: 1,
        }];
        assert!(reader.packed(&p).is_err());
        Ok(())
    }
}
