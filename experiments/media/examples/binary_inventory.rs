use anyhow::Result;
use nenia_media_screen::structure;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::Read,
    path::PathBuf,
};
#[derive(Deserialize)]
struct Input {
    copy: PathBuf,
    provider: String,
}
#[derive(Deserialize)]
struct Corpus {
    files: Vec<Input>,
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let corpus: Corpus = serde_json::from_slice(&fs::read(&args[1])?)?;
    let mut seen = HashSet::new();
    let mut report = BTreeMap::<String, (usize, usize)>::new();
    for file in corpus
        .files
        .into_iter()
        .filter(|f| !f.provider.starts_with("additional_"))
    {
        let source: Box<dyn Read> = if file.copy.extension().is_some_and(|s| s == "zst") {
            Box::new(zstd::stream::read::Decoder::new(File::open(file.copy)?)?)
        } else {
            Box::new(File::open(file.copy)?)
        };
        structure::encode_with_asset_handler(
            source,
            false,
            |_| Ok(0),
            |bytes| {
                if !seen.insert(blake3::hash(bytes)) {
                    return Ok(Some(0));
                }
                let kind = if bytes.starts_with(b"\x89PNG") {
                    "PNG signature"
                } else if bytes.starts_with(b"\xff\xd8\xff") {
                    "JPEG signature"
                } else if bytes.len() >= 73 && bytes[0] == 128 && bytes.len() % 16 == 9 {
                    "Fernet-shaped envelope (heuristic)"
                } else if bytes.starts_with(b"\x28\xb5\x2f\xfd") {
                    "Zstd frame"
                } else if bytes.starts_with(b"\x1f\x8b") {
                    "gzip frame"
                } else if bytes.starts_with(b"PK\x03\x04") {
                    "ZIP signature"
                } else if bytes.starts_with(b"%PDF") {
                    "PDF signature"
                } else if bytes.starts_with(b"RIFF") {
                    "RIFF signature"
                } else if bytes.len() >= 2
                    && bytes[0] == 120
                    && u16::from_be_bytes([bytes[0], bytes[1]]) % 31 == 0
                {
                    "zlib-shaped header"
                } else if std::str::from_utf8(bytes).is_ok() {
                    "UTF-8"
                } else {
                    "unidentified binary"
                };
                let v = report.entry(kind.into()).or_default();
                v.0 += 1;
                v.1 += bytes.len();
                Ok(Some(0))
            },
        )?;
    }
    fs::write(&args[2], serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
