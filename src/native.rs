//! An explicitly versioned, byte-proven inverse for Codex's native Zstd wrapper.
//! Unrecognized encoders, malformed frames and oversized streams stay opaque.
use anyhow::{ensure, Result};
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::path::Path;

pub const PROFILE: &str = "zstd-1.5.7-stream3";
pub const MAX_DECODED: u64 = 2 * 1024 * 1024 * 1024;

pub struct Prepared {
    pub file: File,
    pub original_hash: String,
}

pub fn encoder<W: Write>(out: W, size: u64) -> Result<zstd::stream::write::Encoder<'static, W>> {
    ensure!(zstd::zstd_safe::version_number() == 10507,
        "this snapshot requires the bundled Zstd 1.5.7 reconstruction codec; rebuild with the locked dependencies");
    let mut encoder = zstd::stream::write::Encoder::new(out, 3)?;
    encoder.set_pledged_src_size(Some(size))?;
    Ok(encoder)
}

/// The source is rewound even on an unsupported frame. An anonymous private
/// spool bounds RAM; both encoded and decoded lengths are independently bounded.
pub fn prepare(file: &mut File, size: u64, spool: &Path) -> Result<Option<Prepared>> {
    let attempt = (|| -> Result<Prepared> {
        let mut source = HashRead {
            inner: file.take(size),
            hash: blake3::Hasher::new(),
            bytes: 0,
        };
        let mut decoded = tempfile::tempfile_in(spool)?;
        let count = {
            let mut decoder = zstd::stream::read::Decoder::new(&mut source)?;
            decoder.window_log_max(26)?;
            io::copy(&mut decoder.take(MAX_DECODED + 1), &mut decoded)?
        };
        ensure!(
            count <= MAX_DECODED,
            "native stream exceeds normalization budget"
        );
        io::copy(&mut source, &mut io::sink())?;
        ensure!(source.bytes == size, "native source was truncated");
        let original_hash = source.hash.finalize();
        decoded.rewind()?;
        let mut proof = HashWrite::new(io::sink());
        let mut encoder = encoder(&mut proof, count)?;
        io::copy(&mut decoded, &mut encoder)?;
        encoder.finish()?;
        ensure!(
            proof.bytes == size && proof.hash.finalize() == original_hash,
            "native encoder profile does not reproduce original bytes"
        );
        decoded.rewind()?;
        Ok(Prepared {
            file: decoded,
            original_hash: original_hash.to_hex().to_string(),
        })
    })();
    file.rewind()?;
    // Normalization is optional. A successful opaque capture still propagates
    // real source/storage I/O failures through the ordinary capture path.
    Ok(attempt.ok())
}

struct HashRead<R> {
    inner: R,
    hash: blake3::Hasher,
    bytes: u64,
}
impl<R: Read> Read for HashRead<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(out)?;
        self.hash.update(&out[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
}

pub struct HashWrite<W> {
    pub inner: W,
    pub hash: blake3::Hasher,
    pub bytes: u64,
}
impl<W> HashWrite<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hash: blake3::Hasher::new(),
            bytes: 0,
        }
    }
}
impl<W: Write> Write for HashWrite<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(data)?;
        self.hash.update(&data[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconstruction_profile_is_stable_across_platforms() {
        let mut state = 7u64;
        let input: Vec<u8> = (0..400_003)
            .map(|i| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                if i % 4096 < 3072 {
                    (i % 251) as u8
                } else {
                    (state >> 32) as u8
                }
            })
            .collect();
        let mut out = encoder(Vec::new(), input.len() as u64).unwrap();
        out.write_all(&input).unwrap();
        let encoded = out.finish().unwrap();
        let mut fragmented = encoder(Vec::new(), input.len() as u64).unwrap();
        for part in input.chunks(97) {
            fragmented.write_all(part).unwrap();
        }
        assert_eq!(fragmented.finish().unwrap(), encoded);
        assert_eq!(
            blake3::hash(&encoded).to_hex().as_str(),
            "28147d2dae517593890c33b95913264dad67daf1b9b4631c1487f655218efbf7"
        );
    }
}
