//! Similarity-selected pack dictionaries with a strict one-reference depth.
//! A dictionary is always an ordinary format-1 pack, never another delta pack.
use crate::pack::{self, Chunker, EncodedPack, MAX_PACK_ENCODED_SIZE, MAX_PACK_RAW_SIZE};
use anyhow::{Result, ensure};
use std::{fs, path::Path};
const MAGIC: &[u8; 8] = b"NENIADP1";
struct Anchor {
    id: String,
    features: Vec<u64>,
}
#[derive(Default)]
pub struct Encoder {
    anchors: [Vec<Anchor>; 2],
    pub selected: u64,
    pub saved: u64,
    pub attempts: u64,
}
fn features(raw: &[u8]) -> Result<Vec<u64>> {
    let mut features = Vec::new();
    for part in Chunker::new_small(raw) {
        let hash = blake3::hash(&part?);
        features.push(u64::from_le_bytes(hash.as_bytes()[..8].try_into()?));
    }
    features.sort_unstable();
    features.dedup();
    features.truncate(64);
    Ok(features)
}
fn overlap(a: &[u64], b: &[u64]) -> usize {
    let mut i = 0;
    let mut j = 0;
    let mut same = 0;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
                same += 1;
            }
        }
    }
    same
}
impl Encoder {
    pub fn encode(
        &mut self,
        root: &Path,
        normal: EncodedPack,
        kind: usize,
        level: i32,
    ) -> Result<EncodedPack> {
        if kind > 1 {
            return Ok(normal);
        }
        let raw = pack::decode_pack(&normal.bytes, &normal.id)?;
        let sketch = features(&raw)?;
        let mut candidates = self.anchors[kind]
            .iter()
            .enumerate()
            .map(|(i, a)| (overlap(&sketch, &a.features), i))
            .collect::<Vec<_>>();
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        let mut best = None;
        for &(_, i) in candidates.iter().take(2) {
            self.attempts += 1;
            let anchor = &self.anchors[kind][i];
            let dictionary = pack::decode_pack(
                &fs::read(root.join(format!("{}.nz", anchor.id)))?,
                &anchor.id,
            )?;
            let mut encoder = zstd::bulk::Compressor::with_dictionary(level, &dictionary)?;
            encoder.set_parameter(zstd::zstd_safe::CParameter::WindowLog(23))?;
            encoder.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
            let payload = encoder.compress(&raw)?;
            let mut encoded = MAGIC.to_vec();
            encoded.extend(normal.raw_len.to_le_bytes());
            encoded.extend(blake3::Hash::from_hex(&anchor.id)?.as_bytes());
            encoded.extend(payload);
            // Require a meaningful saving to justify fetching the extra object.
            if encoded.len() * 10 < normal.bytes.len() * 9
                && best
                    .as_ref()
                    .is_none_or(|v: &Vec<u8>| encoded.len() < v.len())
            {
                best = Some(encoded);
            }
        }
        if let Some(bytes) = best {
            let id = blake3::hash(&bytes).to_hex().to_string();
            ensure!(
                decode(root, &bytes, &id)?.0 == raw,
                "dictionary pack round-trip"
            );
            self.selected += 1;
            self.saved += (normal.bytes.len() - bytes.len()) as u64;
            Ok(EncodedPack {
                id,
                bytes,
                raw_len: normal.raw_len,
            })
        } else {
            if self.anchors[kind].len() == 4096 {
                self.anchors[kind].remove(0);
            }
            self.anchors[kind].push(Anchor {
                id: normal.id.clone(),
                features: sketch,
            });
            Ok(normal)
        }
    }
}
pub fn decode(root: &Path, encoded: &[u8], id: &str) -> Result<(Vec<u8>, usize)> {
    if !encoded.starts_with(MAGIC) {
        return Ok((pack::decode_pack(encoded, id)?, 0));
    }
    ensure!(
        encoded.len() > 48
            && encoded.len() <= MAX_PACK_ENCODED_SIZE
            && blake3::hash(encoded).to_hex().as_str() == id,
        "dictionary object identity/size"
    );
    let n = u64::from_le_bytes(encoded[8..16].try_into()?) as usize;
    ensure!(n > 0 && n <= MAX_PACK_RAW_SIZE, "dictionary output budget");
    let anchor = blake3::Hash::from_bytes(encoded[16..48].try_into()?)
        .to_hex()
        .to_string();
    // Calling the legacy decoder here enforces depth one, including for hostile
    // cycles or references to other dictionary packs.
    let dictionary = pack::decode_pack(&fs::read(root.join(format!("{anchor}.nz")))?, &anchor)?;
    let payload = &encoded[48..];
    ensure!(
        zstd::zstd_safe::find_frame_compressed_size(payload)
            .map_err(|e| anyhow::anyhow!("invalid dictionary frame {e}"))?
            == payload.len(),
        "dictionary trailing data"
    );
    let mut decoder = zstd::bulk::Decompressor::with_dictionary(&dictionary)?;
    decoder.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(23))?;
    let raw = decoder.decompress(payload, n)?;
    ensure!(raw.len() == n, "dictionary decoded size");
    Ok((raw, dictionary.len()))
}
