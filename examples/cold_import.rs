//! Compile a measured cold representation into a normal, independently usable
//! Nenia vault. Input traces and previous archives are never modified/deleted.
use anyhow::{ensure, Context, Result};
use nenia::{
    adapters::{self, Source},
    pack::{self, ChunkRef, Chunker, PackBuilder},
    structure::Recipe,
    vault::{Manifest, Vault},
};
use nenia_codecs::{
    media, png,
    shared_pixels::{bind_storage, Prepared},
};
use serde::Deserialize;
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File},
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
    hash: String,
}
#[derive(Deserialize)]
struct OriginalAsset {
    file: String,
}
#[derive(Deserialize)]
struct Asset {
    bytes: usize,
    hash: String,
    codec: u8,
}
#[derive(Deserialize)]
struct Record {
    source: PathBuf,
    provider: String,
    bytes: u64,
    hash: String,
    recipe: Recipe,
    chunks: Vec<ChunkRef>,
    assets: Vec<Asset>,
}
struct Image {
    codec: u16,
    original: PathBuf,
    payload: PathBuf,
    token: Option<Vec<u8>>,
    len: usize,
}
struct Pool {
    root: PathBuf,
    builder: PackBuilder,
    pending: Vec<usize>,
    parts: Vec<ChunkRef>,
    seen: HashMap<blake3::Hash, usize>,
}
impl Pool {
    fn flush(&mut self) -> Result<()> {
        if self.builder.is_empty() {
            return Ok(());
        }
        let p = std::mem::replace(&mut self.builder, PackBuilder::new(9)).finish()?;
        pack::atomic_write_immutable(&self.root.join(format!("{}.nz", p.id)), &p.bytes)?;
        for i in self.pending.drain(..) {
            self.parts[i].pack = p.id.clone();
        }
        Ok(())
    }
    fn put(&mut self, data: &[u8]) -> Result<usize> {
        let hash = blake3::hash(data);
        if let Some(&i) = self.seen.get(&hash) {
            return Ok(i);
        }
        if !self.builder.can_fit(data.len()) {
            self.flush()?;
        }
        let offset = self.builder.add(data)?;
        let i = self.parts.len();
        self.parts.push(ChunkRef {
            hash: String::new(),
            pack: String::new(),
            offset,
            len: data.len() as u32,
        });
        self.pending.push(i);
        self.seen.insert(hash, i);
        Ok(i)
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len()==7,"usage: cold_import PRIVATE_RESEARCH_ARCHIVE PRIVATE_ASSETS CORPUS PRIVATE_PIXEL_PLAN NEW_VAULT AGGREGATE_REPORT");
    let input = Path::new(&args[1]);
    let assets_root = Path::new(&args[2]);
    let out = Path::new(&args[5]);
    ensure!(!out.exists(), "output vault already exists");
    let corpus: Corpus = serde_json::from_slice(&fs::read(&args[3])?)?;
    let corpus = corpus
        .files
        .into_iter()
        .map(|f| ((f.original.clone(), f.hash.clone()), f.copy))
        .collect::<HashMap<_, _>>();
    let inventory: Vec<OriginalAsset> =
        serde_json::from_slice(&fs::read(assets_root.join("assets.json"))?)?;
    let mut originals = HashMap::new();
    for (i, a) in inventory.iter().enumerate() {
        let path = assets_root.join(&a.file);
        let bytes = fs::read(&path)?;
        originals.insert(blake3::hash(&bytes).to_hex().to_string(), (i, path));
    }
    let plan = Prepared::open(Path::new(&args[4]))?;
    let mut files = fs::read_dir(input.join("manifests"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.sort();
    let records = files
        .iter()
        .map(|p| -> Result<Record> {
            Ok(serde_json::from_slice(&zstd::stream::decode_all(
                fs::read(p)?.as_slice(),
            )?)?)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut vault = Vault::init(out, 9)?;
    let staging = tempfile::tempdir_in(out)?;
    let mut images = HashMap::<String, Image>::new();
    let mut leafs = BTreeSet::new();
    let start = Instant::now();
    let mut profiles = std::collections::BTreeMap::<String, u64>::new();
    for record in &records {
        for asset in &record.assets {
            if images.contains_key(&asset.hash) {
                continue;
            }
            let (ordinal, path) = originals
                .get(&asset.hash)
                .context("missing original media")?;
            let original = fs::read(path)?;
            ensure!(
                original.len() == asset.bytes && original.len() <= pack::MAX_PACK_RAW_SIZE,
                "image source budget"
            );
            let (mut payload, token) = if asset.codec == 1 {
                let (payload, stats) = png::encode_portable_structured(&original)?;
                for (key, value) in stats {
                    *profiles.entry(key).or_default() += value;
                }
                let token = if payload[8] == 1 {
                    plan.quote(*ordinal)?
                } else {
                    None
                };
                if let Some((_, objects)) = &token {
                    for (hash, _) in objects {
                        leafs.insert(hash.clone());
                    }
                }
                (payload, token.map(|(token, _)| token))
            } else {
                ensure!(asset.codec == 2, "unknown research asset");
                (media::encode_jpeg(&original)?, None)
            };
            // Verify independently before any manifest can refer to this image.
            if token.is_none() {
                let decoded = if asset.codec == 1 {
                    png::decode(&payload)?
                } else {
                    media::decode_jpeg(&payload)?
                };
                ensure!(decoded == original, "portable image proof");
            }
            let payload_path = staging.path().join(&asset.hash);
            fs::write(&payload_path, &mut payload)?;
            images.insert(
                asset.hash.clone(),
                Image {
                    codec: asset.codec as u16,
                    original: path.clone(),
                    payload: payload_path,
                    token,
                    len: original.len(),
                },
            );
            if images.len().is_multiple_of(50) {
                eprintln!(
                    "{} portable images prepared; {:.1}s",
                    images.len(),
                    start.elapsed().as_secs_f64()
                );
            }
        }
    }
    let mut pool = Pool {
        root: out.join("packs"),
        builder: PackBuilder::new(9),
        pending: Vec::new(),
        parts: Vec::new(),
        seen: HashMap::new(),
    };
    let mut leaf_indices = HashMap::new();
    for hash in &leafs {
        let bytes = fs::read(Path::new(&args[4]).join("objects").join(hash))?;
        ensure!(
            blake3::hash(&bytes).to_hex().as_str() == hash && bytes.len() <= 16 * 1024 * 1024,
            "pixel leaf identity/size"
        );
        let mut parts = Vec::new();
        for chunk in Chunker::new(bytes.as_slice()) {
            parts.push(pool.put(&chunk?)?);
        }
        leaf_indices.insert(hash.clone(), parts);
    }
    pool.flush()?;
    let storage = leaf_indices
        .into_iter()
        .map(|(hash, ids)| {
            (
                hash,
                ids.into_iter()
                    .map(|i| pool.parts[i].clone())
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut image_packs = HashMap::new();
    let mut decoder = media::Decoder::new(out.join("packs"));
    let mut keys = images.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    for hash in keys {
        let image = &images[&hash];
        let mut payload = fs::read(&image.payload)?;
        let mut dependencies = Vec::new();
        if let Some(token) = &image.token {
            let (token, ids) = bind_storage(token, &storage)?;
            payload = png::externalize(&payload, &token)?.context("missing structured PNG")?;
            dependencies = ids;
        }
        let original = fs::read(&image.original)?;
        let p = media::encode(image.codec, &original, &payload, dependencies)?;
        ensure!(
            decoder.decode(&p.bytes, &p.id)? == original,
            "media pack original-byte proof"
        );
        pack::atomic_write_immutable(&out.join("packs").join(format!("{}.nz", p.id)), &p.bytes)?;
        image_packs.insert(hash, (p.id, image.len));
    }
    let mut copied = BTreeSet::new();
    let mut native = 0;
    let mut original_bytes = 0;
    for (ordinal, record) in records.iter().enumerate() {
        let mut chunks = record.chunks.clone();
        for chunk in &chunks {
            if copied.insert(chunk.pack.clone()) {
                let encoded = fs::read(input.join("packs").join(format!("{}.nz", chunk.pack)))?;
                pack::decode_pack(&encoded, &chunk.pack)?;
                pack::atomic_write_immutable(
                    &out.join("packs").join(format!("{}.nz", chunk.pack)),
                    &encoded,
                )?;
            }
        }
        let mut recipe = record.recipe.clone();
        let mut operations = Vec::new();
        let mut asset_refs = HashMap::<usize, Vec<u32>>::new();
        for &[id, offset, len, flags] in &recipe.operations {
            if flags & 64 == 0 {
                operations.push([id, offset, len, flags]);
                continue;
            }
            ensure!(offset == 0, "external asset slice");
            let asset = &record.assets[id as usize];
            ensure!(len as usize == asset.bytes, "external asset size");
            let refs = asset_refs.entry(id as usize).or_insert_with(|| {
                let (pack, n) = &image_packs[&asset.hash];
                let mut refs = Vec::new();
                // All but the final slice are complete base64 triplets. No carry
                // state or source-byte changes are introduced into the recipe.
                for offset in (0..*n).step_by(255 * 1024) {
                    let len = (255 * 1024).min(n - offset);
                    refs.push(chunks.len() as u32);
                    chunks.push(ChunkRef {
                        hash: String::new(),
                        pack: pack.clone(),
                        offset: offset as u64,
                        len: len as u32,
                    });
                }
                refs
            });
            for &index in refs.iter() {
                operations.push([index, 0, chunks[index as usize].len, flags & !64]);
            }
        }
        recipe.operations = operations;
        if recipe.container.is_some() {
            native += 1;
        }
        let copy = corpus
            .get(&(record.source.clone(), record.hash.clone()))
            .context("missing frozen source")?;
        let source = if record.provider.contains("claude") {
            Source::Claude
        } else if record.provider.contains("codex") {
            Source::Codex
        } else {
            Source::Generic
        };
        let metadata = adapters::inspect_source(File::open(copy)?, source, &record.source)?;
        let manifest = Manifest {
            version: 2,
            revision: 0,
            device_id: vault.config.device_id.clone(),
            origin: record.source.to_string_lossy().into_owned(),
            captured_at: chrono::Utc::now().to_rfc3339(),
            content_hash: record.hash.clone(),
            bytes: record.bytes,
            metadata,
            chunks,
            recipe: Some(recipe),
        };
        let encoded = zstd::stream::encode_all(serde_json::to_vec(&manifest)?.as_slice(), 9)?;
        let id = blake3::hash(&encoded).to_hex().to_string();
        nenia::vault::decode_manifest(&encoded, &id)?;
        pack::atomic_write_immutable(
            &out.join("manifests").join(format!("{id}.json.zst")),
            &encoded,
        )?;
        original_bytes += record.bytes;
        if (ordinal + 1).is_multiple_of(100) {
            eprintln!("{} standard manifests compiled", ordinal + 1);
        }
    }
    drop(staging);
    let compile_seconds = start.elapsed().as_secs_f64();
    let validation = Instant::now();
    let rebuilt = vault.rebuild()?;
    ensure!(rebuilt == records.len(), "rebuilt snapshot count");
    let verification = vault.verify()?;
    ensure!(
        verification.snapshots as usize == records.len() && verification.orphan_packs == 0,
        "vault recovery/closure"
    );
    let stats = vault.stats()?;
    let report = serde_json::json!({"candidate":"portable-format-2-media-packs","files":records.len(),"input_bytes":original_bytes,"exact_native_zstd_profiles":native,"unique_media":images.len(),"pixel_leaves":leafs.len(),"ordinary_vault_verification":verification,"catalog_rebuilt_snapshots":rebuilt,"stats":stats,"png_profiles":profiles,"compile_seconds":compile_seconds,"verify_and_catalog_seconds":validation.elapsed().as_secs_f64(),"scope":"Usable Nenia vault, independently verified and rebuilt by the production reader; compilation inputs are private real-data research intermediates"});
    fs::write(&args[6], serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    Ok(())
}
