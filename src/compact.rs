//! Repack cold image bodies without changing or deleting the source vault.
use crate::{
    pack::{self, ChunkRef, Chunker, PackBuilder},
    vault::Vault,
};
use anyhow::{ensure, Result};
use nenia_codecs::media;
use serde::Serialize;
use std::{collections::HashMap, path::Path, time::Instant};
#[derive(Serialize)]
pub struct Report {
    pub images_repacked: usize,
    pub snapshots: usize,
    pub archive_bytes_before: u64,
    pub archive_bytes_after: u64,
    pub seconds: f64,
}
struct Pending {
    old: String,
    codec: u16,
    raw_len: usize,
    body_hash: String,
    body_len: usize,
    parts: Vec<usize>,
    dependencies: Vec<String>,
}
pub fn run(source: &Vault, destination: &Path) -> Result<Report> {
    ensure!(!destination.exists(), "compact output already exists");
    let start = Instant::now();
    let before = source.stats()?.archive_bytes;
    let mut output = Vault::init(destination, source.config.compression_level)?;
    let inventory = source.inventory()?;
    let mut builder = PackBuilder::new(9);
    let mut waiting = Vec::<usize>::new();
    let mut refs = Vec::<ChunkRef>::new();
    let mut seen = HashMap::new();
    let mut pending = Vec::new();
    let flush = |builder: &mut PackBuilder,
                 waiting: &mut Vec<usize>,
                 refs: &mut Vec<ChunkRef>|
     -> Result<()> {
        if builder.is_empty() {
            return Ok(());
        }
        let p = std::mem::replace(builder, PackBuilder::new(9)).finish()?;
        pack::atomic_write_immutable(
            &destination.join("packs").join(format!("{}.nz", p.id)),
            &p.bytes,
        )?;
        for i in waiting.drain(..) {
            refs[i].pack = p.id.clone();
        }
        Ok(())
    };
    for id in &inventory.packs {
        let encoded = source.object("packs", id)?;
        media::preflight(&encoded, id)?;
        if media::is_media(&encoded) {
            let h = media::header(&encoded)?;
            if h.codec != 3 && h.payload.len() >= 1024 {
                let mut parts = Vec::new();
                for bytes in Chunker::new(h.payload) {
                    let bytes = bytes?;
                    let hash = blake3::hash(&bytes);
                    let part = if let Some(&i) = seen.get(&hash) {
                        i
                    } else {
                        if !builder.can_fit(bytes.len()) {
                            flush(&mut builder, &mut waiting, &mut refs)?;
                        }
                        let offset = builder.add(&bytes)?;
                        let i = refs.len();
                        refs.push(ChunkRef {
                            hash: String::new(),
                            pack: String::new(),
                            offset,
                            len: bytes.len() as u32,
                        });
                        waiting.push(i);
                        seen.insert(hash, i);
                        i
                    };
                    parts.push(part);
                }
                pending.push(Pending {
                    old: id.clone(),
                    codec: h.codec,
                    raw_len: h.raw_len,
                    body_hash: blake3::hash(h.payload).to_hex().to_string(),
                    body_len: h.payload.len(),
                    parts,
                    dependencies: h.dependencies,
                });
                continue;
            }
        }
        pack::atomic_write_immutable(
            &destination.join("packs").join(format!("{id}.nz")),
            &encoded,
        )?;
    }
    flush(&mut builder, &mut waiting, &mut refs)?;
    let images_repacked = pending.len();
    let mut mapping = HashMap::new();
    let mut decoder = media::Decoder::new(destination.join("packs"));
    for p in pending {
        let parts = p.parts.iter().map(|&i| refs[i].clone()).collect::<Vec<_>>();
        let encoded = media::referenced(
            p.codec,
            p.raw_len,
            &p.body_hash,
            p.body_len,
            &parts,
            &p.dependencies,
        )?;
        decoder.decode(&encoded.bytes, &encoded.id)?;
        pack::atomic_write_immutable(
            &destination.join("packs").join(format!("{}.nz", encoded.id)),
            &encoded.bytes,
        )?;
        mapping.insert(p.old, encoded.id);
    }
    for id in &inventory.manifests {
        let mut m = source.manifest(id)?;
        for c in &mut m.chunks {
            if let Some(id) = mapping.get(&c.pack) {
                c.pack = id.clone();
            }
        }
        let bytes = zstd::stream::encode_all(serde_json::to_vec(&m)?.as_slice(), 9)?;
        let id = blake3::hash(&bytes).to_hex().to_string();
        crate::vault::decode_manifest(&bytes, &id)?;
        pack::atomic_write_immutable(
            &destination.join("manifests").join(format!("{id}.json.zst")),
            &bytes,
        )?;
    }
    let snapshots = output.rebuild()?;
    ensure!(
        snapshots == inventory.manifests.len(),
        "compact snapshot count"
    );
    let after = output.stats()?.archive_bytes;
    // Only the new vault is written. The source remains available for rollback.
    Ok(Report {
        images_repacked,
        snapshots,
        archive_bytes_before: before,
        archive_bytes_after: after,
        seconds: start.elapsed().as_secs_f64(),
    })
}
