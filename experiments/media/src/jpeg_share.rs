//! Shared AC coefficient blocks, independent DC columns, exact source JPEGs.
use anyhow::{Result, ensure};
use nenia_media_screen::jpeg::{self, Coefficients};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};
const BLOCKS: usize = 65536;
const AC_BYTES: usize = 126;
#[derive(Deserialize)]
struct Asset {
    file: String,
    kind: String,
    cohort: String,
}
#[derive(Serialize, Deserialize)]
struct Index {
    version: u32,
    packs: Vec<(String, usize)>,
}
struct Store {
    root: PathBuf,
    index: Index,
    seen: HashMap<blake3::Hash, u32>,
    raw: Vec<u8>,
    bytes: u64,
    transpose: bool,
}
impl Store {
    fn flush(&mut self) -> Result<()> {
        if self.raw.is_empty() {
            return Ok(());
        }
        let count = self.raw.len() / AC_BYTES;
        let raw = if self.transpose {
            let mut out = Vec::with_capacity(self.raw.len());
            for k in 0..63 {
                for block in self.raw.chunks_exact(AC_BYTES) {
                    out.extend_from_slice(&block[k * 2..k * 2 + 2]);
                }
            }
            out
        } else {
            self.raw.clone()
        };
        let encoded = zstd::stream::encode_all(raw.as_slice(), 9)?;
        let id = self.index.packs.len();
        fs::write(self.root.join(format!("{id:06}.coefficients")), &encoded)?;
        self.bytes += encoded.len() as u64;
        self.index
            .packs
            .push((blake3::hash(&encoded).to_hex().to_string(), count));
        self.raw.clear();
        Ok(())
    }
    fn add(&mut self, block: &[i16; 64]) -> Result<u32> {
        let mut bytes = [0; AC_BYTES];
        for (value, out) in block[1..].iter().zip(bytes.chunks_exact_mut(2)) {
            out.copy_from_slice(&value.to_le_bytes());
        }
        let hash = blake3::hash(&bytes);
        if let Some(&id) = self.seen.get(&hash) {
            return Ok(id);
        }
        if self.raw.len() == BLOCKS * AC_BYTES {
            self.flush()?;
        }
        let id = (self.index.packs.len() * BLOCKS + self.raw.len() / AC_BYTES) as u32;
        self.raw.extend(bytes);
        self.seen.insert(hash, id);
        Ok(id)
    }
}
fn number(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u32).to_le_bytes());
}
fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    ensure!(n <= input.len(), "truncated coefficient recipe");
    let out = &input[..n];
    *input = &input[n..];
    Ok(out)
}
fn integer(input: &mut &[u8]) -> Result<usize> {
    Ok(u32::from_le_bytes(take(input, 4)?.try_into()?) as usize)
}
fn blob(out: &mut Vec<u8>, bytes: &[u8]) {
    number(out, bytes.len());
    out.extend(bytes);
}
fn section<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let n = integer(input)?;
    take(input, n)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 5,
        "usage: jpeg-share PRIVATE_ASSETS PRIVATE_OUTPUT REPORT row|column"
    );
    let input = Path::new(&args[1]);
    let root = Path::new(&args[2]);
    ensure!(!root.exists(), "output exists");
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    let transpose = match args[4].as_str() {
        "row" => false,
        "column" => true,
        _ => anyhow::bail!("coefficient layout"),
    };
    let assets: Vec<Asset> = serde_json::from_slice(&fs::read(input.join("assets.json"))?)?;
    let mut store = Store {
        root: root.into(),
        index: Index {
            version: if transpose { 2 } else { 1 },
            packs: Vec::new(),
        },
        seen: HashMap::new(),
        raw: Vec::new(),
        bytes: 0,
        transpose,
    };
    let mut samples = Vec::new();
    let mut recipes = 0;
    let mut original = None;
    let mut total_blocks = 0;
    let mut failures = 0;
    let mut native_bytes = 0;
    let mut checkpoint_packs = 0;
    let start = Instant::now();
    for (id, asset) in assets.iter().enumerate() {
        if asset.cohort != "original" && original.is_none() {
            store.flush()?;
            let index_bytes =
                zstd::stream::encode_all(serde_json::to_vec(&store.index)?.as_slice(), 9)?.len()
                    as u64;
            checkpoint_packs = store.index.packs.len();
            original = Some(
                serde_json::json!({"images":samples.len(),"native_bytes":native_bytes,"blocks":total_blocks,"unique_ac_blocks":store.seen.len(),"pack_bytes":store.bytes,"recipe_bytes":recipes,"index_bytes":index_bytes,"container_bytes":store.bytes+recipes+index_bytes,"fallbacks":failures}),
            );
        }
        if asset.kind != "jpeg" {
            continue;
        }
        let data = fs::read(input.join(&asset.file))?;
        native_bytes += data.len() as u64;
        let coefficients = jpeg::decode(&data).and_then(|v| {
            ensure!(jpeg::encode(&v)? == data, "noncanonical JPEG encoding");
            Ok(v)
        });
        let mut body = Vec::new();
        let mut mode = 0;
        let mut count = 0;
        if let Ok(coefficients) = coefficients {
            mode = 1;
            blob(&mut body, &coefficients.header);
            blob(&mut body, &coefficients.tail);
            number(&mut body, coefficients.blocks.len());
            count = coefficients.blocks.len();
            total_blocks += count;
            for block in &coefficients.blocks {
                body.extend(store.add(block)?.to_le_bytes());
            }
            for block in &coefficients.blocks {
                body.extend(block[0].to_le_bytes());
            }
        } else {
            body = data.clone();
            failures += 1;
        }
        let compressed = zstd::stream::encode_all(body.as_slice(), 9)?;
        let mut frame = b"NENIACF1".to_vec();
        frame.push(mode);
        number(&mut frame, data.len());
        frame.extend(blake3::hash(&data).as_bytes());
        frame.extend(compressed);
        fs::write(root.join(format!("{id:06}.recipe")), &frame)?;
        recipes += frame.len() as u64;
        samples.push(serde_json::json!({"ordinal":id,"cohort":asset.cohort,"original_bytes":data.len(),"recipe_bytes":frame.len(),"blocks":count,"mode":mode}));
        if samples.len().is_multiple_of(100) {
            eprintln!(
                "{} JPEG recipes; {} unique AC blocks; {:.1}s",
                samples.len(),
                store.seen.len(),
                start.elapsed().as_secs_f64()
            );
        }
    }
    store.flush()?;
    let index_raw = serde_json::to_vec(&store.index)?;
    let index = zstd::stream::encode_all(index_raw.as_slice(), 9)?;
    fs::write(root.join("index.zstd"), &index)?;
    let index_bytes = index.len() as u64;
    let pack_bytes = store.bytes;
    let unique = store.seen.len();
    let packs = store.index.packs.len();
    let encode_seconds = start.elapsed().as_secs_f64();
    drop(store);
    let index: Index = serde_json::from_slice(&zstd::stream::decode_all(
        fs::read(root.join("index.zstd"))?.as_slice(),
    )?)?;
    let start = Instant::now();
    let mut cache = VecDeque::<(usize, Vec<u8>)>::new();
    let mut read_bytes = 0u64;
    for sample in &samples {
        let id = sample["ordinal"].as_u64().unwrap() as usize;
        let file = fs::read(root.join(format!("{id:06}.recipe")))?;
        ensure!(&file[..8] == b"NENIACF1", "recipe signature");
        let size = u32::from_le_bytes(file[9..13].try_into()?) as usize;
        let body = zstd::stream::decode_all(&file[45..])?;
        let restored = if file[8] == 0 {
            body
        } else {
            ensure!(file[8] == 1, "recipe codec");
            let mut body = body.as_slice();
            let header = section(&mut body)?.to_vec();
            let tail = section(&mut body)?.to_vec();
            let count = integer(&mut body)?;
            ensure!(
                count <= 1_048_576 && body.len() == count * 6,
                "recipe coefficient bounds"
            );
            let references = take(&mut body, count * 4)?;
            let dc = body;
            let mut blocks = Vec::with_capacity(count);
            for (ordinal, id) in references.chunks_exact(4).enumerate() {
                let id = u32::from_le_bytes(id.try_into()?) as usize;
                let pack = id / BLOCKS;
                let offset = id % BLOCKS;
                ensure!(
                    pack < index.packs.len() && offset < index.packs[pack].1,
                    "coefficient reference bounds"
                );
                if let Some(position) = cache.iter().position(|(i, _)| *i == pack) {
                    let hit = cache.remove(position).unwrap();
                    cache.push_front(hit);
                } else {
                    let encoded = fs::read(root.join(format!("{pack:06}.coefficients")))?;
                    ensure!(
                        blake3::hash(&encoded).to_hex().as_str() == index.packs[pack].0,
                        "coefficient pack hash"
                    );
                    let raw = zstd::stream::decode_all(encoded.as_slice())?;
                    ensure!(
                        raw.len() == index.packs[pack].1 * AC_BYTES,
                        "coefficient pack bounds"
                    );
                    read_bytes += raw.len() as u64;
                    if cache.len() == 16 {
                        cache.pop_back();
                    }
                    cache.push_front((pack, raw));
                }
                let raw = &cache.front().unwrap().1;
                let mut block = [0; 64];
                block[0] = i16::from_le_bytes(dc[ordinal * 2..ordinal * 2 + 2].try_into()?);
                for (k, value) in block[1..].iter_mut().enumerate() {
                    let pos = if index.version == 2 {
                        (k * index.packs[pack].1 + offset) * 2
                    } else {
                        offset * AC_BYTES + k * 2
                    };
                    *value = i16::from_le_bytes(raw[pos..pos + 2].try_into()?);
                }
                blocks.push(block);
            }
            jpeg::encode(&Coefficients {
                header,
                tail,
                blocks,
            })?
        };
        ensure!(
            restored.len() == size
                && blake3::hash(&restored).as_bytes() == &file[13..45]
                && restored == fs::read(input.join(&assets[id].file))?,
            "exact JPEG byte restore failed"
        );
    }
    let report = serde_json::json!({"candidate":"shared-quantized-AC-coefficients","scope":"Complete JPEG collection archive including fallback bytes and dictionary; one exploratory trial","layout":args[4],"original":original,"images":samples.len(),"original_bytes":native_bytes,"blocks":total_blocks,"unique_ac_blocks":unique,"packs":packs,"original_packs":checkpoint_packs,"pack_bytes":pack_bytes,"recipe_bytes":recipes,"index_bytes":index_bytes,"container_bytes":pack_bytes+recipes+index_bytes,"fallbacks":failures,"exact_restores":samples.len(),"encode_seconds":encode_seconds,"restore_with_oracle_seconds":start.elapsed().as_secs_f64(),"cache_decoded_bytes":read_bytes,"samples":samples});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}",
        serde_json::json!({"images":samples.len(),"bytes":pack_bytes+recipes+index_bytes,"unique_ac_blocks":unique,"fallbacks":failures})
    );
    Ok(())
}
