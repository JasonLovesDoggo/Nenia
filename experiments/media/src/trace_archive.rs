//! Complete trace archives with external reversible media. Actual frozen inputs
//! only. Asset preparation is a separate measured phase, never hidden in timing.
use anyhow::{Result, ensure};
use nenia_media_screen::{
    pack::{self, ChunkRef, Chunker, PackBuilder},
    png, shared_pixels,
    structure::{self, Recipe},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    time::Instant,
};
#[derive(Deserialize)]
struct Corpus {
    files: Vec<Input>,
}
#[derive(Deserialize)]
struct Input {
    copy: PathBuf,
    original: PathBuf,
    provider: String,
    bytes: u64,
    hash: String,
}
#[derive(Deserialize)]
struct Inventory {
    file: String,
    kind: String,
}
#[derive(Serialize, Deserialize, Clone)]
struct Asset {
    bytes: usize,
    hash: String,
    encoded_hash: String,
    codec: u8,
    chunks: Vec<ChunkRef>,
}
#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    source: PathBuf,
    provider: String,
    bytes: u64,
    hash: String,
    recipe: Recipe,
    chunks: Vec<ChunkRef>,
    assets: Vec<Asset>,
}
struct Store {
    root: PathBuf,
    builders: [PackBuilder; 4],
    pending: [Vec<usize>; 4],
    refs: Vec<ChunkRef>,
    seen: HashMap<blake3::Hash, usize>,
    bytes: u64,
    typed: bool,
    unique_classes: [u64; 4],
    encoded_classes: [u64; 4],
}
impl Store {
    fn new(root: PathBuf, typed: bool) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            builders: std::array::from_fn(|_| PackBuilder::new(9)),
            pending: std::array::from_fn(|_| Vec::new()),
            refs: Vec::new(),
            seen: HashMap::new(),
            bytes: 0,
            typed,
            unique_classes: [0; 4],
            encoded_classes: [0; 4],
        })
    }
    fn flush_one(&mut self, kind: usize) -> Result<()> {
        if self.builders[kind].is_empty() {
            return Ok(());
        }
        let p = std::mem::replace(&mut self.builders[kind], PackBuilder::new(9)).finish()?;
        pack::atomic_write_immutable(&self.root.join(format!("{}.nz", p.id)), &p.bytes)?;
        for id in self.pending[kind].drain(..) {
            self.refs[id].pack = p.id.clone();
        }
        self.bytes += p.bytes.len() as u64;
        self.encoded_classes[kind] += p.bytes.len() as u64;
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        for kind in 0..4 {
            self.flush_one(kind)?;
        }
        Ok(())
    }
    fn put(&mut self, data: &[u8], kind: usize) -> Result<usize> {
        let hash = blake3::hash(data);
        if let Some(&id) = self.seen.get(&hash) {
            return Ok(id);
        }
        self.unique_classes[kind] += data.len() as u64;
        let kind = if self.typed { kind } else { 0 };
        if !self.builders[kind].can_fit(data.len()) {
            self.flush_one(kind)?;
        }
        let offset = self.builders[kind].add(data)?;
        let id = self.refs.len();
        self.refs.push(ChunkRef {
            hash: String::new(),
            pack: String::new(),
            offset,
            len: data.len() as u32,
        });
        self.pending[kind].push(id);
        self.seen.insert(hash, id);
        Ok(id)
    }
}
struct Cache {
    root: PathBuf,
    entries: VecDeque<(String, Vec<u8>)>,
    decoded: u64,
}
impl Cache {
    fn get(&mut self, chunk: &ChunkRef) -> Result<&[u8]> {
        if let Some(i) = self.entries.iter().position(|(id, _)| id == &chunk.pack) {
            let p = self.entries.remove(i).unwrap();
            self.entries.push_front(p);
        } else {
            let raw = pack::decode_pack(
                &fs::read(self.root.join(format!("{}.nz", chunk.pack)))?,
                &chunk.pack,
            )?;
            self.decoded += raw.len() as u64;
            if self.entries.len() == 8 {
                self.entries.pop_back();
            }
            self.entries.push_front((chunk.pack.clone(), raw));
        }
        pack::extract_chunk(&self.entries.front().unwrap().1, chunk)
    }
}
fn asset_decode(codec: u8, data: &[u8], pixels: &mut shared_pixels::Reader) -> Result<Vec<u8>> {
    if codec == 1 {
        return png::decode_with_pixels(data, |token| pixels.resolve(token));
    }
    ensure!(
        codec == 2 && data.len() >= 45 && &data[..8] == b"NENIAJP1",
        "JPEG container header"
    );
    let size = u32::from_le_bytes(data[9..13].try_into()?) as usize;
    ensure!(size <= 32 * 1024 * 1024, "JPEG output limit");
    let mut out = Vec::new();
    if data[8] == 0 {
        let mut d = zstd::stream::read::Decoder::new(&data[45..])?;
        d.window_log_max(25)?;
        d.take(32 * 1024 * 1024 + 1).read_to_end(&mut out)?;
    } else {
        ensure!(data[8] == 1, "JPEG codec");
        let features = lepton_jpeg::EnabledFeatures {
            max_threads: 1,
            max_jpeg_width: 8192,
            max_jpeg_height: 8192,
            max_jpeg_file_size: 32 * 1024 * 1024,
            ..lepton_jpeg::EnabledFeatures::compat_lepton_vector_write()
        };
        lepton_jpeg::decode_lepton(
            &mut io::Cursor::new(&data[45..]),
            &mut out,
            &features,
            &lepton_jpeg::SingleThreadPool::default(),
        )?;
    }
    ensure!(
        out.len() == size && blake3::hash(&out).as_bytes() == &data[13..45],
        "JPEG identity"
    );
    Ok(out)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        (8..=10).contains(&args.len()),
        "usage: trace-archive CORPUS PRIVATE_ASSETS PRIVATE_PNG_OR_DASH PRIVATE_JPEG_OR_DASH PRIVATE_OUTPUT REPORT original|all [PRIVATE_PIXEL_STORE_OR_DASH] [typed]"
    );
    let mut corpus: Corpus = serde_json::from_slice(&fs::read(&args[1])?)?;
    corpus.files.sort_by_key(|f| {
        (
            f.provider.starts_with("additional_"),
            f.provider.clone(),
            f.copy.clone(),
        )
    });
    if args[7] == "original" {
        corpus
            .files
            .retain(|f| !f.provider.starts_with("additional_"));
    } else {
        ensure!(args[7] == "all", "cohort");
    }
    let root = Path::new(&args[5]);
    ensure!(!root.exists(), "output exists");
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    fs::create_dir(root.join("manifests"))?;
    let inventory: Vec<Inventory> =
        serde_json::from_slice(&fs::read(Path::new(&args[2]).join("assets.json"))?)?;
    let mut prepared = HashMap::new();
    for (i, asset) in inventory.iter().enumerate() {
        let (folder, codec) = if asset.kind == "png" {
            (&args[3], 1)
        } else {
            (&args[4], 2)
        };
        if folder == "-" {
            continue;
        }
        let path = Path::new(folder).join(format!("{i:06}.asset"));
        if !path.exists() {
            continue;
        }
        let source = fs::read(Path::new(&args[2]).join(&asset.file))?;
        prepared.insert(blake3::hash(&source), (path, codec, i));
    }
    let prepared_pixels = args
        .get(8)
        .filter(|p| p.as_str() != "-")
        .map(|p| shared_pixels::Prepared::open(Path::new(p)))
        .transpose()?;
    let mut pixels = shared_pixels::Reader::new(root.join("pixels"));
    let typed = args.get(9).is_some_and(|s| s == "typed");
    let mut store = Store::new(root.join("packs"), typed)?;
    let mut media = Store::new(root.join("media"), false)?;
    let mut saved = HashMap::<blake3::Hash, usize>::new();
    let mut global_assets = Vec::<(Asset, Vec<usize>)>::new();
    let mut records = Vec::new();
    let start = Instant::now();
    let mut bytes = 0;
    let mut native = 0;
    let mut asset_occurrences = 0;
    for (ordinal, input) in corpus.files.iter().enumerate() {
        let compressed = input.copy.extension().is_some_and(|s| s == "zst");
        let reader: Box<dyn Read> = if compressed {
            let mut d = zstd::stream::read::Decoder::new(File::open(&input.copy)?)?;
            d.window_log_max(26)?;
            Box::new(d)
        } else {
            Box::new(File::open(&input.copy)?)
        };
        let mut local = HashMap::new();
        let mut ids = Vec::new();
        let mut local_assets = HashMap::new();
        let mut asset_ids = Vec::new();
        let mut recipe = structure::encode_with_typed_assets(
            reader,
            false,
            |part, flags| {
                let kind = if flags & 8 != 0 {
                    if flags & 16 != 0 { 3 } else { 2 }
                } else if flags != 0 {
                    1
                } else {
                    0
                };
                let global = store.put(part, kind)?;
                Ok(*local.entry(global).or_insert_with(|| {
                    ids.push(global);
                    ids.len() as u32 - 1
                }))
            },
            |data| {
                if !(data.starts_with(b"\x89PNG\r\n\x1a\n") || data.starts_with(b"\xff\xd8\xff")) {
                    return Ok(None);
                }
                let hash = blake3::hash(data);
                let Some((path, codec, ordinal)) = prepared.get(&hash) else {
                    return Ok(None);
                };
                asset_occurrences += 1;
                let global = if let Some(&id) = saved.get(&hash) {
                    id
                } else {
                    let mut encoded = fs::read(path)?;
                    if *codec == 1
                        && encoded.get(8) == Some(&1)
                        && let Some(prepared_pixels) = &prepared_pixels
                        && let Some(token) =
                            prepared_pixels.materialize(*ordinal, &root.join("pixels"))?
                    {
                        encoded = png::externalize(&encoded, &token)?.expect("transformed PNG");
                    }
                    ensure!(
                        asset_decode(*codec, &encoded, &mut pixels)? == data,
                        "prepared asset differs"
                    );
                    let mut parts = Vec::new();
                    for part in Chunker::new(encoded.as_slice()) {
                        parts.push(media.put(&part?, 0)?);
                    }
                    let id = global_assets.len();
                    global_assets.push((
                        Asset {
                            bytes: data.len(),
                            hash: hash.to_hex().to_string(),
                            encoded_hash: blake3::hash(&encoded).to_hex().to_string(),
                            codec: *codec,
                            chunks: Vec::new(),
                        },
                        parts,
                    ));
                    saved.insert(hash, id);
                    id
                };
                Ok(Some(*local_assets.entry(global).or_insert_with(|| {
                    asset_ids.push(global);
                    asset_ids.len() as u32 - 1
                })))
            },
        )?;
        if compressed {
            recipe.container = Some("zstd-1.5.7-stream3".into());
            native += 1;
        }
        bytes += input.bytes;
        records.push((
            Record {
                version: 3,
                source: input.original.clone(),
                provider: input.provider.clone(),
                bytes: input.bytes,
                hash: input.hash.clone(),
                recipe,
                chunks: Vec::new(),
                assets: Vec::new(),
            },
            ids,
            asset_ids,
        ));
        if (ordinal + 1).is_multiple_of(100) {
            eprintln!("{} trace recipes", ordinal + 1);
        }
    }
    store.flush()?;
    media.flush()?;
    for (asset, ids) in &mut global_assets {
        asset.chunks = ids.iter().map(|&i| media.refs[i].clone()).collect();
    }
    let mut manifest_bytes = 0;
    for (i, (record, ids, assets)) in records.iter_mut().enumerate() {
        record.chunks = ids.iter().map(|&i| store.refs[i].clone()).collect();
        record.assets = assets.iter().map(|&i| global_assets[i].0.clone()).collect();
        let encoded = zstd::stream::encode_all(serde_json::to_vec(record)?.as_slice(), 9)?;
        manifest_bytes += encoded.len() as u64;
        fs::write(root.join("manifests").join(format!("{i:06}.zstd")), encoded)?;
    }
    let pixel_bytes = if root.join("pixels").exists() {
        fs::read_dir(root.join("pixels"))?
            .map(|p| p.and_then(|p| p.metadata()).map(|m| m.len()))
            .collect::<std::io::Result<Vec<_>>>()?
            .iter()
            .sum::<u64>()
    } else {
        0
    };
    let payload_bytes = store.bytes;
    let media_bytes = media.bytes;
    let assets = global_assets.len();
    let encode_seconds = start.elapsed().as_secs_f64();
    let unique_classes = store.unique_classes;
    let encoded_classes = store.encoded_classes;
    drop(records);
    drop(global_assets);
    drop(saved);
    drop(store);
    drop(media);
    drop(prepared);
    let start = Instant::now();
    let mut pixels = shared_pixels::Reader::new(root.join("pixels"));
    let mut cache = Cache {
        root: root.join("packs"),
        entries: VecDeque::new(),
        decoded: 0,
    };
    let mut media_cache = Cache {
        root: root.join("media"),
        entries: VecDeque::new(),
        decoded: 0,
    };
    let mut image_cache = VecDeque::<(String, Vec<u8>)>::new();
    for (i, input) in corpus.files.iter().enumerate() {
        let raw = zstd::stream::decode_all(
            fs::read(root.join("manifests").join(format!("{i:06}.zstd")))?.as_slice(),
        )?;
        let record: Record = serde_json::from_slice(&raw)?;
        ensure!(
            record.version == 3 && record.hash == input.hash && record.bytes == input.bytes,
            "manifest identity"
        );
        let mut output = tempfile::tempfile_in(root)?;
        let mut logical = blake3::Hasher::new();
        let mut logical_bytes = 0;
        let mut replay = |out: &mut dyn Write| -> Result<()> {
            for [id, offset, len, flags] in &record.recipe.operations {
                let expanded = if flags & 64 != 0 {
                    let asset = &record.assets[*id as usize];
                    ensure!(*offset == 0 && *len as usize == asset.bytes, "asset bounds");
                    if let Some(pos) = image_cache.iter().position(|(hash, _)| hash == &asset.hash)
                    {
                        let hit = image_cache.remove(pos).unwrap();
                        image_cache.push_front(hit);
                    } else {
                        let mut encoded = Vec::new();
                        for chunk in &asset.chunks {
                            encoded.extend_from_slice(media_cache.get(chunk)?);
                        }
                        ensure!(
                            blake3::hash(&encoded).to_hex().as_str() == asset.encoded_hash,
                            "asset encoded hash"
                        );
                        let decoded = asset_decode(asset.codec, &encoded, &mut pixels)?;
                        ensure!(
                            decoded.len() == asset.bytes
                                && blake3::hash(&decoded).to_hex().as_str() == asset.hash,
                            "asset original hash"
                        );
                        while !image_cache.is_empty()
                            && image_cache.iter().map(|(_, v)| v.len()).sum::<usize>()
                                + decoded.len()
                                > 128 * 1024 * 1024
                        {
                            image_cache.pop_back();
                        }
                        image_cache.push_front((asset.hash.clone(), decoded));
                    }
                    structure::expand(&image_cache.front().unwrap().1, flags & !64)?
                } else {
                    let raw = cache.get(&record.chunks[*id as usize])?;
                    structure::expand(&raw[*offset as usize..(*offset + *len) as usize], *flags)?
                };
                logical.update(&expanded);
                logical_bytes += expanded.len() as u64;
                out.write_all(&expanded)?;
            }
            Ok(())
        };
        if record.recipe.container.is_some() {
            ensure!(zstd::zstd_safe::version_number() == 10507, "native version");
            let mut encoder = zstd::stream::write::Encoder::new(&mut output, 3)?;
            encoder.set_pledged_src_size(Some(record.recipe.decoded_bytes))?;
            replay(&mut encoder)?;
            encoder.finish()?;
        } else {
            replay(&mut output)?;
        }
        ensure!(
            logical_bytes == record.recipe.decoded_bytes
                && logical.finalize().to_hex().as_str() == record.recipe.decoded_hash,
            "logical trace identity"
        );
        ensure!(
            output.stream_position()? == record.bytes,
            "native trace size"
        );
        output.rewind()?;
        let mut source = File::open(&input.copy)?;
        let mut hash = blake3::Hasher::new();
        let mut a = [0; 65536];
        let mut b = [0; 65536];
        loop {
            let n = output.read(&mut a)?;
            if n == 0 {
                ensure!(source.read(&mut b)? == 0, "source tail");
                break;
            }
            source.read_exact(&mut b[..n])?;
            ensure!(a[..n] == b[..n], "bytewise trace mismatch at ordinal {i}");
            hash.update(&a[..n]);
        }
        ensure!(
            hash.finalize().to_hex().as_str() == record.hash,
            "native trace hash"
        );
        if (i + 1).is_multiple_of(100) {
            eprintln!("{} exact trace restores", i + 1);
        }
    }
    let report = serde_json::json!({"candidate":"reversible-media-trace-archive","scope":"Complete trace bytes and source metadata; no disposable SQLite index; asset preprocessing time excluded and reported separately; one exploratory trial","files":corpus.files.len(),"input_bytes":bytes,"archive_bytes":payload_bytes+media_bytes+manifest_bytes+pixel_bytes,"text_pack_bytes":payload_bytes,"media_pack_bytes":media_bytes,"pixel_object_bytes":pixel_bytes,"manifest_bytes":manifest_bytes,"unique_assets":assets,"asset_occurrences":asset_occurrences,"typed_packing":typed,"class_order":["literal","text","base64_standard","base64_url"],"unique_class_bytes":unique_classes,"encoded_class_bytes":encoded_classes,"exact_restores":corpus.files.len(),"exact_native_zstd_restores":native,"encode_with_prepared_assets_seconds":encode_seconds,"restore_with_bytewise_oracle_seconds":start.elapsed().as_secs_f64(),"text_cache_decoded_bytes":cache.decoded,"media_cache_decoded_bytes":media_cache.decoded});
    fs::write(&args[6], serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    Ok(())
}
