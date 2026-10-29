//! Screening harness on frozen real inputs; contains no generated workload.
use anyhow::{ensure, Result};
use nenia::{
    pack::{self, ChunkRef, PackBuilder},
    structure::{self, Recipe},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{Read, Write},
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
    provider: String,
    bytes: u64,
    hash: String,
}
#[derive(Serialize)]
struct Record {
    version: u32,
    bytes: u64,
    content_hash: String,
    recipe: Recipe,
    chunks: Vec<ChunkRef>,
}
struct Store {
    root: PathBuf,
    builder: PackBuilder,
    pending: Vec<usize>,
    refs: Vec<ChunkRef>,
    seen: HashMap<blake3::Hash, usize>,
    encoded: u64,
    target: usize,
    level: i32,
}
impl Store {
    fn flush(&mut self) -> Result<()> {
        if self.builder.is_empty() {
            return Ok(());
        }
        let p = std::mem::replace(&mut self.builder, PackBuilder::new(self.level)).finish()?;
        pack::atomic_write_immutable(&self.root.join(format!("{}.nz", p.id)), &p.bytes)?;
        for i in self.pending.drain(..) {
            self.refs[i].pack = p.id.clone();
        }
        self.encoded += p.bytes.len() as u64;
        Ok(())
    }
    fn put(&mut self, bytes: &[u8]) -> Result<usize> {
        let hash = blake3::hash(bytes);
        if let Some(id) = self.seen.get(&hash) {
            return Ok(*id);
        }
        if !self.builder.can_fit(bytes.len()) || self.builder.len() + bytes.len() > self.target {
            self.flush()?;
        }
        let offset = self.builder.add(bytes)?;
        let id = self.refs.len();
        self.refs.push(ChunkRef {
            hash: hash.to_hex().to_string(),
            pack: String::new(),
            offset,
            len: bytes.len() as u32,
        });
        self.pending.push(id);
        self.seen.insert(hash, id);
        Ok(id)
    }
}
struct Cache {
    root: PathBuf,
    entries: VecDeque<(String, Vec<u8>)>,
    decompressed: u64,
    hits: u64,
    misses: u64,
}
impl Cache {
    fn get(&mut self, chunk: &ChunkRef) -> Result<&[u8]> {
        if let Some(index) = self.entries.iter().position(|(id, _)| id == &chunk.pack) {
            let entry = self.entries.remove(index).unwrap();
            self.entries.push_front(entry);
            self.hits += 1;
        } else {
            let raw = pack::decode_pack(
                &fs::read(self.root.join(format!("{}.nz", chunk.pack)))?,
                &chunk.pack,
            )?;
            self.decompressed += raw.len() as u64;
            self.misses += 1;
            if self.entries.len() == 16 {
                self.entries.pop_back();
            }
            self.entries.push_front((chunk.pack.clone(), raw));
        }
        pack::extract_chunk(&self.entries.front().unwrap().1, chunk)
    }
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() >= 4,
        "usage: structure_probe CORPUS PRIVATE_OUTPUT REPORT [original|all] [pack_bytes] [level]"
    );
    let corpus: Corpus = serde_json::from_reader(File::open(&args[1])?)?;
    let root = Path::new(&args[2]);
    ensure!(!root.exists(), "output exists");
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    fs::create_dir(root.join("packs"))?;
    fs::create_dir(root.join("manifests"))?;
    let target = args
        .get(5)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(8 * 1024 * 1024);
    let level = args.get(6).map(|s| s.parse()).transpose()?.unwrap_or(9);
    let mut store = Store {
        root: root.join("packs"),
        builder: PackBuilder::new(level),
        pending: Vec::new(),
        refs: Vec::new(),
        seen: HashMap::new(),
        encoded: 0,
        target,
        level,
    };
    let mut records = Vec::new();
    let mut input_bytes = 0u64;
    let mut decoded_bytes = 0u64;
    let mut ops = 0u64;
    let mut native_files = 0;
    let start = Instant::now();
    for input in corpus.files {
        if args.get(4).is_some_and(|s| s == "original") && input.provider == "additional_claude" {
            continue;
        }
        let native = input.copy.extension().is_some_and(|s| s == "zst");
        let mut reader: Box<dyn Read> = if native {
            let mut decoder = zstd::stream::read::Decoder::new(File::open(&input.copy)?)?;
            decoder.window_log_max(26)?;
            Box::new(decoder)
        } else {
            Box::new(File::open(&input.copy)?)
        };
        let mut local = HashMap::new();
        let mut ids = Vec::new();
        let mut recipe = structure::encode_with_policy(
            &mut reader,
            args.get(7).is_some_and(|s| s == "small"),
            |bytes| {
                let global = store.put(bytes)?;
                Ok(*local.entry(global).or_insert_with(|| {
                    ids.push(global);
                    ids.len() as u32 - 1
                }))
            },
        )?;
        if native {
            recipe.container = Some("zstd-1.5.7-stream3".into());
            native_files += 1;
        }
        input_bytes += input.bytes;
        decoded_bytes += recipe.decoded_bytes;
        ops += recipe.operations.len() as u64;
        records.push((
            Record {
                version: 2,
                bytes: input.bytes,
                content_hash: input.hash,
                recipe,
                chunks: Vec::new(),
            },
            ids,
        ));
        if records.len() % 50 == 0 {
            eprintln!(
                "encoded {} real files, {} pack bytes",
                records.len(),
                store.encoded
            );
        }
    }
    store.flush()?;
    let mut manifest_bytes = 0u64;
    for (i, (record, ids)) in records.iter_mut().enumerate() {
        record.chunks = ids.iter().map(|i| store.refs[*i].clone()).collect();
        let raw = serde_json::to_vec(record)?;
        let encoded = zstd::stream::encode_all(raw.as_slice(), 9)?;
        manifest_bytes += encoded.len() as u64;
        pack::atomic_write_immutable(
            &root.join("manifests").join(format!("{i}.json.zst")),
            &encoded,
        )?;
    }
    let encode_seconds = start.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({"phase":"encoded","files":records.len(),"archive_bytes":store.encoded+manifest_bytes,"pack_bytes":store.encoded,"manifest_bytes":manifest_bytes,"encode_seconds":encode_seconds})
    );
    let start = Instant::now();
    let mut cache = Cache {
        root: root.join("packs"),
        entries: VecDeque::new(),
        decompressed: 0,
        hits: 0,
        misses: 0,
    };
    for (i, (record, _)) in records.iter().enumerate() {
        let mut hash = blake3::Hasher::new();
        let mut logical = blake3::Hasher::new();
        let mut logical_bytes = 0;
        let mut replay = |out: &mut dyn Write| -> Result<()> {
            for [id, offset, len, flags] in &record.recipe.operations {
                let raw = cache.get(&record.chunks[*id as usize])?;
                let expanded =
                    structure::expand(&raw[*offset as usize..(*offset + *len) as usize], *flags)?;
                logical.update(&expanded);
                logical_bytes += expanded.len() as u64;
                out.write_all(&expanded)?;
            }
            Ok(())
        };
        if record.recipe.container.is_some() {
            let mut encoder = zstd::stream::write::Encoder::new(&mut hash, 3)?;
            encoder.set_pledged_src_size(Some(record.recipe.decoded_bytes))?;
            replay(&mut encoder)?;
            encoder.finish()?;
        } else {
            replay(&mut hash)?;
        }
        ensure!(
            logical_bytes == record.recipe.decoded_bytes
                && logical.finalize().to_hex().as_str() == record.recipe.decoded_hash,
            "decoded restore failed"
        );
        ensure!(
            hash.finalize().to_hex().as_str() == record.content_hash,
            "native restore failed at ordinal {i}"
        );
        if (i + 1) % 50 == 0 {
            eprintln!("verified {} real files", i + 1);
        }
    }
    let report = serde_json::json!({"candidate":"lexical-sharing-screen","files":records.len(),"input_bytes":input_bytes,"decoded_bytes":decoded_bytes,"archive_bytes":store.encoded+manifest_bytes,"pack_bytes":store.encoded,"manifest_bytes":manifest_bytes,"unique_chunks":store.refs.len(),"unique_chunk_bytes":store.refs.iter().map(|c|c.len as u64).sum::<u64>(),"operations":ops,"encode_seconds":encode_seconds,"restore_seconds":start.elapsed().as_secs_f64(),"exact_restores":records.len(),"exact_native_zstd_restores":native_files,"pack_target":target,"level":level,"cache_decompressed_bytes":cache.decompressed,"cache_hits":cache.hits,"cache_misses":cache.misses,"limitations":"screening harness; no catalog; manifest omits origin metadata; native recipe proven during restore rather than ingest; final production benchmark required"});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    Ok(())
}
