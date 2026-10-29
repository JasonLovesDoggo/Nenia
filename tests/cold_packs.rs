//! Public correctness fixtures only; performance evidence uses private traces.
use anyhow::Result;
use nenia::{
    adapters::{Metadata, Source},
    pack::{self, ChunkRef, PackBuilder},
    vault::{Manifest, Vault},
};
use nenia_codecs::{media, png};
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(kind);
    out.extend(data);
    let mut crc = 0xffff_ffffu32;
    for &b in kind.iter().chain(data) {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320u32.wrapping_mul(crc & 1));
        }
    }
    out.extend((!crc).to_be_bytes());
}
fn fixture() -> Result<Vec<u8>> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(1u32.to_be_bytes());
    header.extend(1u32.to_be_bytes());
    header.extend([8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    // A fixed public zlib stream for filter byte 0 and RGB [1,2,3].
    // Use the format-independent PNG fixture bytes; zlib 78 01, stored block.
    let zlib = [
        0x78, 0x01, 0x01, 0x04, 0x00, 0xfb, 0xff, 0, 1, 2, 3, 0, 14, 0, 7,
    ];
    chunk(&mut out, b"IDAT", &zlib);
    chunk(&mut out, b"IEND", &[]);
    Ok(out)
}
fn manifest(original: &[u8], p: &pack::EncodedPack) -> Manifest {
    Manifest {
        version: 1,
        revision: 0,
        device_id: "public-unit-fixture".into(),
        origin: "unit.png".into(),
        captured_at: "2026-01-01T00:00:00Z".into(),
        content_hash: blake3::hash(original).to_hex().to_string(),
        bytes: original.len() as u64,
        metadata: Metadata {
            source: Source::Generic,
            ..Default::default()
        },
        chunks: vec![ChunkRef {
            hash: blake3::hash(original).to_hex().to_string(),
            pack: p.id.clone(),
            offset: 0,
            len: original.len() as u32,
        }],
        recipe: None,
    }
}
#[test]
fn media_dependency_upload_is_order_independent_but_manifest_commit_is_not() -> Result<()> {
    let original = fixture()?;
    let (payload, _) = png::encode_portable_structured(&original)?;
    assert_eq!(payload[8], 1);
    assert_eq!(png::decode(&payload)?, original);
    let pixels = [1u8, 2, 3];
    let encoded = zstd::stream::encode_all(pixels.as_slice(), 9)?;
    let mut builder = PackBuilder::new(9);
    builder.add(&encoded)?;
    let leaf = builder.finish()?;
    let token = serde_json::json!({"Delta":{"frame":{"width":1,"height":1,"channels":3,"codec":0,"hash":blake3::hash(&encoded).to_hex().to_string(),"storage":[[leaf.id,0,encoded.len()]]},"base":null,"dy":0,"subtract":false,"hash":blake3::hash(&pixels).to_hex().to_string()}});
    let token = zstd::stream::encode_all(serde_json::to_vec(&token)?.as_slice(), 9)?;
    let payload = png::externalize(&payload, &token)?.unwrap();
    let image = media::encode(1, &original, &payload, [leaf.id.clone()])?;
    let dir = tempfile::tempdir()?;
    let mut vault = Vault::init(dir.path(), 9)?;
    vault.receive_object("packs", &image.id, &image.bytes)?;
    let m = manifest(&original, &image);
    let encoded = zstd::stream::encode_all(serde_json::to_vec(&m)?.as_slice(), 9)?;
    let id = blake3::hash(&encoded).to_hex().to_string();
    assert!(vault.receive_object("manifests", &id, &encoded).is_err());
    assert!(vault.list(None, 10, true)?.is_empty());
    vault.receive_object("packs", &leaf.id, &leaf.bytes)?;
    vault.receive_object("manifests", &id, &encoded)?;
    let report = vault.verify()?;
    assert_eq!(report.snapshots, 1);
    assert_eq!(report.packs, 2);
    assert_eq!(report.orphan_packs, 0);
    assert_eq!(vault.rebuild()?, 1);
    let mut bad = image.bytes.clone();
    bad[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    let bad_id = blake3::hash(&bad).to_hex().to_string();
    assert!(media::preflight(&bad, &bad_id).is_err());
    Ok(())
}
#[test]
fn portable_format_rejects_legacy_prediction_codec() -> Result<()> {
    let original = fixture()?;
    let (mut payload, _) = png::encode_portable_structured(&original)?;
    payload[45] = 1;
    assert!(png::decode(&payload).is_err());
    let (payload, _) = png::encode_portable(b"opaque incomplete PNG signature")?;
    let image = media::encode(1, b"opaque incomplete PNG signature", &payload, [])?;
    let dir = tempfile::tempdir()?;
    assert_eq!(
        media::Decoder::new(dir.path().into()).decode(&image.bytes, &image.id)?,
        b"opaque incomplete PNG signature"
    );
    Ok(())
}

#[test]
fn compact_keeps_source_and_restores_referenced_bodies() -> Result<()> {
    let mut state = 17u64;
    let original = (0..4096)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect::<Vec<_>>();
    let (payload, _) = png::encode_portable(&original)?;
    let image = media::encode(1, &original, &payload, [])?;
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    let destination = temp.path().join("compact");
    let mut vault = Vault::init(&source, 9)?;
    vault.receive_object("packs", &image.id, &image.bytes)?;
    let m = manifest(&original, &image);
    let bytes = zstd::stream::encode_all(serde_json::to_vec(&m)?.as_slice(), 9)?;
    let id = blake3::hash(&bytes).to_hex().to_string();
    vault.receive_object("manifests", &id, &bytes)?;
    let before = vault.inventory()?;
    let report = nenia::compact::run(&vault, &destination)?;
    assert_eq!(report.images_repacked, 1);
    assert_eq!(vault.inventory()?.packs, before.packs);
    assert!(nenia::compact::run(&vault, &source).is_err());
    let compact = Vault::open(&destination)?;
    let snapshots = compact.list(None, 10, true)?;
    let mut restored = Vec::new();
    compact.export(&snapshots[0].id, &mut restored)?;
    assert_eq!(restored, original);
    assert_eq!(compact.verify()?.orphan_packs, 0);
    Ok(())
}
