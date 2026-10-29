//! Content-defined chunks and bounded, immutable Zstd packs.
//!
//! A pack is a 20-byte little-endian header followed by a Zstd frame. The header
//! contains an eight-byte magic, a u16 version, u16 reserved flags, and a u64 raw
//! length. Chunk offsets address the uncompressed concatenation. Both object and
//! chunk IDs are BLAKE3 hashes; a catalog is never needed to interpret a pack.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;

pub const MIN_CHUNK_SIZE: usize = 8 * 1024;
pub const AVG_CHUNK_SIZE: usize = 32 * 1024;
pub const MAX_CHUNK_SIZE: usize = 256 * 1024;
pub const MAX_PACK_RAW_SIZE: usize = 8 * 1024 * 1024;
/// Includes room for the worst-case Zstd overhead of an incompressible pack.
pub const MAX_PACK_ENCODED_SIZE: usize = MAX_PACK_RAW_SIZE + 64 * 1024;

const MAGIC: &[u8; 8] = b"NENIAPK\0";
const VERSION: u16 = 1;
const HEADER_SIZE: usize = 20;
const WINDOW_LOG: u32 = 23;
// Including the mandatory minimum, a uniformly distributed fingerprint has an
// expected chunk length of 32 KiB (slightly less because of the maximum bound).
const CUT_THRESHOLD: u64 = u64::MAX / (AVG_CHUNK_SIZE - MIN_CHUNK_SIZE) as u64;
const GEAR: [u64; 256] = gear_table();

const fn gear_table() -> [u64; 256] {
    let mut table = [0; 256];
    let mut state = 0x4e45_4e49_415f_4344_u64;
    let mut i = 0;
    while i < table.len() {
        // Fixed SplitMix64 table: stable across machines, processes and builds.
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = value ^ (value >> 31);
        i += 1;
    }
    table
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredChunk", into = "StoredChunk")]
pub struct ChunkRef {
    pub hash: String,
    pub pack: String,
    pub offset: u64,
    pub len: u32,
}

// Version 2 omits the redundant plaintext hash. Pack authentication and the
// snapshot's final hash cover every referenced byte; the disposable dedup index
// recomputes chunk hashes during recovery. Legacy references keep their hashes.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StoredChunk {
    Compact(String, u64, u32),
    Legacy {
        hash: String,
        pack: String,
        offset: u64,
        len: u32,
    },
}
impl From<StoredChunk> for ChunkRef {
    fn from(value: StoredChunk) -> Self {
        match value {
            StoredChunk::Compact(pack, offset, len) => Self {
                hash: String::new(),
                pack,
                offset,
                len,
            },
            StoredChunk::Legacy {
                hash,
                pack,
                offset,
                len,
            } => Self {
                hash,
                pack,
                offset,
                len,
            },
        }
    }
}
impl From<ChunkRef> for StoredChunk {
    fn from(c: ChunkRef) -> Self {
        if c.hash.is_empty() {
            Self::Compact(c.pack, c.offset, c.len)
        } else {
            Self::Legacy {
                hash: c.hash,
                pack: c.pack,
                offset: c.offset,
                len: c.len,
            }
        }
    }
}

/// Streaming Gear-hash chunker with bounded memory and deterministic boundaries.
///
/// The rolling fingerprint forgets bytes after 64 positions. Insertions therefore
/// allow boundaries to converge again, unlike fixed-offset blocks. A trailing
/// chunk may be smaller than `MIN_CHUNK_SIZE`; empty inputs yield no chunks.
pub struct Chunker<R: Read> {
    reader: BufReader<R>,
    done: bool,
    fixed: bool,
    small: bool,
}

impl<R: Read> Chunker<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::with_capacity(AVG_CHUNK_SIZE, reader),
            done: false,
            fixed: false,
            small: false,
        }
    }

    /// Finer boundaries for extracted values, whose JSON wrappers no longer
    /// interrupt matches. Archive readers do not depend on chunking policy.
    pub fn new_small(reader: R) -> Self {
        let mut chunker = Self::new(reader);
        chunker.small = true;
        chunker
    }

    /// Sealed compressed streams rarely share shifted plaintext chunks. Larger
    /// fixed blocks avoid rolling-hash work and excess catalog entries for them.
    pub fn new_fixed(reader: R) -> Self {
        let mut chunker = Self::new(reader);
        chunker.fixed = true;
        chunker
    }

    pub fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.done {
            return Ok(None);
        }
        if self.fixed {
            let mut chunk = Vec::with_capacity(MAX_CHUNK_SIZE);
            if let Err(error) = self
                .reader
                .by_ref()
                .take(MAX_CHUNK_SIZE as u64)
                .read_to_end(&mut chunk)
            {
                self.done = true;
                return Err(error);
            }
            self.done = chunk.len() < MAX_CHUNK_SIZE;
            return Ok((!chunk.is_empty()).then_some(chunk));
        }
        let mut chunk = Vec::with_capacity(AVG_CHUNK_SIZE);
        let mut fingerprint = 0_u64;
        let (min, max, threshold) = if self.small {
            (512, 16 * 1024, u64::MAX / 1536)
        } else {
            (MIN_CHUNK_SIZE, MAX_CHUNK_SIZE, CUT_THRESHOLD)
        };
        loop {
            let available = match self.reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.done = true;
                    return Err(error);
                }
            };
            if available.is_empty() {
                self.done = true;
                return Ok((!chunk.is_empty()).then_some(chunk));
            }

            let mut consumed = 0;
            let mut boundary = false;
            for &byte in available {
                fingerprint = (fingerprint << 1).wrapping_add(GEAR[byte as usize]);
                consumed += 1;
                let len = chunk.len() + consumed;
                if len == max || (len >= min && fingerprint < threshold) {
                    boundary = true;
                    break;
                }
            }
            chunk.extend_from_slice(&available[..consumed]);
            self.reader.consume(consumed);
            if boundary {
                return Ok(Some(chunk));
            }
        }
    }
}

impl<R: Read> Iterator for Chunker<R> {
    type Item = io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_chunk().transpose()
    }
}

#[derive(Debug)]
pub struct EncodedPack {
    pub id: String,
    pub bytes: Vec<u8>,
    pub raw_len: u64,
}

/// A bounded group of raw chunks compressed together for cross-chunk redundancy.
/// Deduplication belongs to the caller, which already maintains the chunk index.
pub struct PackBuilder {
    raw: Vec<u8>,
    level: i32,
}

impl PackBuilder {
    pub fn new(level: i32) -> Self {
        Self {
            raw: Vec::new(),
            level,
        }
    }

    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    pub fn can_fit(&self, len: usize) -> bool {
        len > 0 && len <= MAX_CHUNK_SIZE && len <= MAX_PACK_RAW_SIZE - self.raw.len()
    }

    /// Append a chunk, returning its offset in the uncompressed pack.
    pub fn add(&mut self, chunk: &[u8]) -> Result<u64> {
        ensure!(
            self.can_fit(chunk.len()),
            "chunk is empty, too large, or pack is full"
        );
        let offset = self.raw.len() as u64;
        self.raw.extend_from_slice(chunk);
        Ok(offset)
    }

    pub fn finish(self) -> Result<EncodedPack> {
        ensure!(!self.raw.is_empty(), "cannot encode an empty pack");
        let raw_len = self.raw.len() as u64;
        let mut bytes = Vec::with_capacity(HEADER_SIZE);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&raw_len.to_le_bytes());
        let mut encoder = zstd::stream::write::Encoder::new(bytes, self.level)
            .context("create Zstd pack encoder")?;
        encoder.window_log(WINDOW_LOG)?;
        encoder.include_checksum(true)?;
        encoder.set_pledged_src_size(Some(raw_len))?;
        encoder.write_all(&self.raw)?;
        let bytes = encoder.finish().context("finish Zstd pack")?;
        ensure!(
            bytes.len() <= MAX_PACK_ENCODED_SIZE,
            "encoded pack exceeds size limit"
        );
        Ok(EncodedPack {
            id: blake3::hash(&bytes).to_hex().to_string(),
            bytes,
            raw_len,
        })
    }
}

/// Decode an authenticated pack while bounding output and Zstd's window memory.
pub fn decode_pack(encoded: &[u8], expected_id: &str) -> Result<Vec<u8>> {
    validate_hash_id(expected_id)?;
    ensure!(
        encoded.len() > HEADER_SIZE && encoded.len() <= MAX_PACK_ENCODED_SIZE,
        "encoded pack length is outside bounds"
    );
    ensure!(
        blake3::hash(encoded).to_hex().as_str() == expected_id,
        "pack hash mismatch"
    );
    ensure!(&encoded[..8] == MAGIC, "invalid pack magic");
    ensure!(
        u16::from_le_bytes(encoded[8..10].try_into()?) == VERSION,
        "unsupported pack version"
    );
    ensure!(encoded[10..12] == [0, 0], "unsupported pack flags");
    let raw_len = u64::from_le_bytes(encoded[12..20].try_into()?);
    ensure!(
        raw_len > 0 && raw_len <= MAX_PACK_RAW_SIZE as u64,
        "raw pack length is outside bounds"
    );
    let mut decoder = zstd::stream::read::Decoder::with_buffer(&encoded[HEADER_SIZE..])
        .context("open Zstd pack")?
        .single_frame();
    decoder.window_log_max(WINDOW_LOG)?;
    let mut raw = vec![0; raw_len as usize];
    decoder.read_exact(&mut raw).context("decompress pack")?;
    // Finish the frame to verify its checksum and reject excess output without
    // ever growing the allocation beyond the declared, bounded raw size.
    ensure!(
        decoder
            .read(&mut [0_u8; 1])
            .context("finish decoding pack")?
            == 0,
        "raw pack length mismatch"
    );
    ensure!(
        decoder.finish().is_empty(),
        "trailing data after pack frame"
    );
    Ok(raw)
}

/// Extract and verify one chunk from an already authenticated, decoded pack.
/// The caller must have passed `reference.pack` to `decode_pack`.
pub fn extract_chunk<'a>(raw: &'a [u8], reference: &ChunkRef) -> Result<&'a [u8]> {
    if !reference.hash.is_empty() {
        validate_hash_id(&reference.hash)?;
    }
    validate_hash_id(&reference.pack)?;
    ensure!(
        raw.len() <= MAX_PACK_RAW_SIZE,
        "raw pack exceeds size limit"
    );
    ensure!(
        reference.len > 0 && reference.len as usize <= MAX_CHUNK_SIZE,
        "invalid chunk length"
    );
    let start =
        usize::try_from(reference.offset).context("chunk offset does not fit address space")?;
    let end = start
        .checked_add(reference.len as usize)
        .context("chunk range overflow")?;
    let chunk = raw.get(start..end).context("chunk range is outside pack")?;
    ensure!(
        reference.hash.is_empty() || blake3::hash(chunk).to_hex().as_str() == reference.hash,
        "chunk hash mismatch"
    );
    Ok(chunk)
}

pub fn validate_hash_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "object ID must be 64 lowercase hexadecimal characters"
    );
    Ok(())
}

/// Publish bytes without replacing an existing object. Returns `false` if the
/// destination already contains exactly these bytes; differing contents fail.
/// The file is flushed before publication and its parent directory is synced.
pub fn atomic_write_immutable(path: &Path, bytes: &[u8]) -> Result<bool> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => {
            sync_directory(parent)?;
            Ok(true)
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            // Windows FlushFileBuffers requires a handle opened for writing,
            // even though verification itself never modifies existing bytes.
            let mut existing = OpenOptions::new()
                .read(true)
                .write(cfg!(windows))
                .open(path)?;
            ensure!(
                existing.metadata()?.len() == bytes.len() as u64,
                "existing object has different length: {}",
                path.display()
            );
            let mut buffer = [0_u8; 64 * 1024];
            for expected in bytes.chunks(buffer.len()) {
                existing.read_exact(&mut buffer[..expected.len()])?;
                if &buffer[..expected.len()] != expected {
                    bail!("existing object has different contents: {}", path.display());
                }
            }
            ensure!(
                existing.read(&mut buffer[..1])? == 0,
                "existing object changed while checking: {}",
                path.display()
            );
            // Also complete publication durability when a prior attempt linked
            // the file successfully but failed while syncing its directory.
            existing.sync_all()?;
            sync_directory(parent)?;
            Ok(false)
        }
        Err(error) => Err(error.error).with_context(|| format!("publish {}", path.display())),
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn random_bytes(len: usize) -> Vec<u8> {
        let mut state = 0x1234_5678_9abc_def0_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn chunks(data: &[u8]) -> Vec<Vec<u8>> {
        Chunker::new(data).collect::<io::Result<Vec<_>>>().unwrap()
    }

    fn encode(data: &[u8]) -> EncodedPack {
        let mut builder = PackBuilder::new(3);
        for chunk in data.chunks(MAX_CHUNK_SIZE) {
            builder.add(chunk).unwrap();
        }
        builder.finish().unwrap()
    }

    fn decode(bytes: &[u8]) -> Result<Vec<u8>> {
        decode_pack(bytes, blake3::hash(bytes).to_hex().as_str())
    }

    #[test]
    fn chunking_preserves_arbitrary_bytes_with_bounds() {
        for len in [
            0,
            1,
            MIN_CHUNK_SIZE - 1,
            MIN_CHUNK_SIZE,
            AVG_CHUNK_SIZE + 1,
            MAX_CHUNK_SIZE * 3 + 17,
        ] {
            let data = random_bytes(len);
            let chunks = chunks(&data);
            assert_eq!(chunks.concat(), data);
            for (index, chunk) in chunks.iter().enumerate() {
                assert!(!chunk.is_empty() && chunk.len() <= MAX_CHUNK_SIZE);
                if index + 1 < chunks.len() {
                    assert!(chunk.len() >= MIN_CHUNK_SIZE);
                }
            }
        }
        let repeated = vec![b'a'; MAX_CHUNK_SIZE * 2 + 7];
        assert_eq!(chunks(&repeated).concat(), repeated);
    }

    #[test]
    fn fixed_chunks_preserve_empty_exact_and_partial_blocks() {
        for len in [0, 1, MAX_CHUNK_SIZE, MAX_CHUNK_SIZE * 3 + 17] {
            let data = random_bytes(len);
            let got = Chunker::new_fixed(data.as_slice())
                .collect::<io::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(got.concat(), data);
            assert_eq!(got.len(), len.div_ceil(MAX_CHUNK_SIZE));
            assert!(got
                .iter()
                .all(|chunk| !chunk.is_empty() && chunk.len() <= MAX_CHUNK_SIZE));
        }
    }

    #[test]
    fn insertion_reuses_existing_chunks() {
        let original = random_bytes(3 * 1024 * 1024);
        let mut edited = original.clone();
        edited.splice(
            200_000..200_000,
            b"a newly inserted event\n".iter().copied(),
        );
        let old: HashSet<_> = chunks(&original)
            .iter()
            .map(|chunk| blake3::hash(chunk))
            .collect();
        let new: HashSet<_> = chunks(&edited)
            .iter()
            .map(|chunk| blake3::hash(chunk))
            .collect();
        assert!(old.intersection(&new).count() * 10 >= old.len() * 9);
    }

    #[test]
    fn input_fragmentation_and_interrupts_do_not_change_boundaries() {
        struct Fragmented<'a> {
            data: &'a [u8],
            interrupt: bool,
        }
        impl Read for Fragmented<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if self.interrupt {
                    self.interrupt = false;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let limit = buffer.len().min(17);
                self.data.read(&mut buffer[..limit])
            }
        }
        let data = random_bytes(MAX_CHUNK_SIZE * 2);
        let fragmented = Chunker::new(Fragmented {
            data: &data,
            interrupt: true,
        })
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
        assert_eq!(fragmented, chunks(&data));
        let fixed = Chunker::new_fixed(Fragmented {
            data: &data,
            interrupt: true,
        })
        .collect::<io::Result<Vec<_>>>()
        .unwrap();
        assert_eq!(
            fixed,
            data.chunks(MAX_CHUNK_SIZE)
                .map(|c| c.to_vec())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn pack_roundtrip_and_verified_chunk_extraction() {
        let data = random_bytes(MAX_CHUNK_SIZE * 3 + 123);
        let mut builder = PackBuilder::new(3);
        let mut references = Vec::new();
        for chunk in chunks(&data) {
            let offset = builder.add(&chunk).unwrap();
            references.push(ChunkRef {
                hash: blake3::hash(&chunk).to_hex().to_string(),
                pack: String::new(),
                offset,
                len: chunk.len() as u32,
            });
        }
        let encoded = builder.finish().unwrap();
        let raw = decode_pack(&encoded.bytes, &encoded.id).unwrap();
        assert_eq!(raw, data);
        assert_eq!(encoded.raw_len, data.len() as u64);
        let mut restored = Vec::new();
        for reference in &mut references {
            reference.pack = encoded.id.clone();
            restored.extend_from_slice(extract_chunk(&raw, reference).unwrap());
        }
        assert_eq!(restored, data);
        let reference = &mut references[0];
        reference.hash = "0".repeat(64);
        assert!(extract_chunk(&raw, reference).is_err());
        reference.offset = u64::MAX;
        assert!(extract_chunk(&raw, reference).is_err());
    }

    #[test]
    fn rejects_corruption_truncation_and_invalid_headers() {
        let encoded = encode(&random_bytes(128 * 1024));
        let mut corrupt = encoded.bytes.clone();
        *corrupt.last_mut().unwrap() ^= 0x80;
        assert!(decode_pack(&corrupt, &encoded.id).is_err());
        assert!(decode(&corrupt).is_err());
        for len in [0, 7, HEADER_SIZE, HEADER_SIZE + 3, encoded.bytes.len() - 1] {
            assert!(decode(&encoded.bytes[..len]).is_err());
        }
        for index in [0, 8, 10] {
            let mut corrupt = encoded.bytes.clone();
            corrupt[index] ^= 0x80;
            assert!(decode(&corrupt).is_err());
        }
        for length in [
            0,
            1,
            128 * 1024 - 1,
            128 * 1024 + 1,
            MAX_PACK_RAW_SIZE as u64 + 1,
            u64::MAX,
        ] {
            let mut corrupt = encoded.bytes.clone();
            corrupt[12..20].copy_from_slice(&length.to_le_bytes());
            assert!(decode(&corrupt).is_err());
        }
        let mut trailing = encoded.bytes.clone();
        trailing.extend_from_slice(&zstd::stream::encode_all(&b""[..], 1).unwrap());
        assert!(decode(&trailing).is_err());
    }

    #[test]
    fn pack_limits_and_deterministic_encoding() {
        let mut builder = PackBuilder::new(1);
        assert!(!builder.can_fit(0));
        assert!(!builder.can_fit(MAX_CHUNK_SIZE + 1));
        assert!(!builder.can_fit(usize::MAX));
        assert!(builder.add(&[]).is_err());
        let chunk = random_bytes(MAX_CHUNK_SIZE);
        for _ in 0..MAX_PACK_RAW_SIZE / MAX_CHUNK_SIZE {
            builder.add(&chunk).unwrap();
        }
        assert!(!builder.can_fit(1));
        assert!(builder.add(&[0]).is_err());
        let encoded = builder.finish().unwrap();
        assert_eq!(
            decode_pack(&encoded.bytes, &encoded.id).unwrap().len(),
            MAX_PACK_RAW_SIZE
        );
        assert!(PackBuilder::new(1).finish().is_err());
        assert_eq!(encode(&chunk).bytes, encode(&chunk).bytes);
    }

    #[test]
    fn immutable_write_reuses_identical_but_never_clobbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("packs").join("object.nz");
        assert!(atomic_write_immutable(&path, b"original").unwrap());
        assert!(!atomic_write_immutable(&path, b"original").unwrap());
        assert!(atomic_write_immutable(&path, b"changed!").is_err());
        assert!(atomic_write_immutable(&path, b"longer contents").is_err());
        assert_eq!(fs::read(path).unwrap(), b"original");
    }

    #[test]
    fn object_ids_are_strictly_lowercase_hex() {
        assert!(validate_hash_id(&"a".repeat(64)).is_ok());
        for bad in [
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(63),
            "a".repeat(65),
            "../".repeat(22),
        ] {
            assert!(validate_hash_id(&bad).is_err());
        }
    }
}
