use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine};
use nenia::{
    adapters::{Candidate, Metadata, Source},
    pack::{ChunkRef, PackBuilder},
    vault::{Manifest, Vault},
};
use std::{fs, io::Write, path::Path};

fn candidate(path: &Path) -> Candidate {
    Candidate {
        path: path.into(),
        source: Source::Auto,
    }
}
fn content() -> Vec<u8> {
    let text = "searchableopal Unicode é𐀀\nquoted \\\" value\t".repeat(400);
    let nested =
        serde_json::to_string(&serde_json::json!({"output":text,"new_provider_field":true}))
            .unwrap();
    let binary: Vec<_> = (0..320_017).map(|n| (n * 71 % 251) as u8).collect();
    let mut raw = format!(
        "{{\"unknown\":1e+00,\"text\":{},\"nested\":{},\"data\":\"{}\"}}\r\n",
        serde_json::to_string(&text).unwrap(),
        serde_json::to_string(&nested).unwrap(),
        STANDARD.encode(binary)
    )
    .into_bytes();
    raw.extend_from_slice(b"{\"noncanonical\":\"\\u0061\\/\"}\r\ninvalid\xff\x00\n{\"partial\":");
    raw
}
fn native(raw: &[u8], checksum: bool) -> Vec<u8> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder
        .set_pledged_src_size(Some(raw.len() as u64))
        .unwrap();
    encoder.include_checksum(checksum).unwrap();
    encoder.write_all(raw).unwrap();
    encoder.finish().unwrap()
}
fn manifest_bytes(m: &Manifest) -> (String, Vec<u8>) {
    let bytes = zstd::encode_all(serde_json::to_vec(m).unwrap().as_slice(), 9).unwrap();
    (blake3::hash(&bytes).to_hex().to_string(), bytes)
}

#[test]
fn raw_and_native_share_payload_and_recover_without_sources_or_catalog() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("vault");
    let raw = content();
    let encoded = native(&raw, false);
    let plain = dir.path().join("source.jsonl");
    let sealed = dir.path().join("source.jsonl.zst");
    fs::write(&plain, &raw)?;
    fs::write(&sealed, &encoded)?;
    let mut vault = Vault::init(&root, 3)?;
    let first = vault.ingest(&[candidate(&plain)], Source::Auto)?;
    let second = vault.ingest(&[candidate(&sealed)], Source::Auto)?;
    assert_eq!(second.new_pack_bytes, 0);
    assert_eq!(second.new_chunk_bytes, 0);
    let a = &first.snapshots[0];
    let b = &second.snapshots[0];
    assert_eq!(vault.manifest(a)?.version, 2);
    let mut decoded = Vec::new();
    vault.export_decoded(b, &mut decoded)?;
    assert_eq!(decoded, raw);
    let manifest = vault.manifest(b)?;
    assert_eq!(
        manifest.recipe.unwrap().container.as_deref(),
        Some("zstd-1.5.7-stream3")
    );
    assert!(manifest.chunks.iter().all(|c| c.hash.is_empty()));
    vault.index(1_000_000)?;
    assert_eq!(vault.search("searchableopal", 10, true)?.len(), 2);
    let before = vault.stats()?.archive_bytes;
    let unchanged = vault.ingest(&[candidate(&plain), candidate(&sealed)], Source::Auto)?;
    assert_eq!(unchanged.unchanged, 2);
    assert_eq!(unchanged.new_pack_bytes, 0);
    assert_eq!(vault.stats()?.archive_bytes, before);
    let mut remote = Vault::init(&dir.path().join("remote"), 3)?;
    assert!(remote
        .receive_object("manifests", b, &vault.object("manifests", b)?)
        .is_err());
    for id in vault.inventory()?.packs {
        remote.receive_object("packs", &id, &vault.object("packs", &id)?)?;
    }
    for id in [a, b] {
        remote.receive_object("manifests", id, &vault.object("manifests", id)?)?;
    }
    for (id, expected) in [(a, &raw), (b, &encoded)] {
        let mut output = Vec::new();
        remote.export(id, &mut output)?;
        assert_eq!(&output, expected);
    }
    fs::remove_file(plain)?;
    fs::remove_file(sealed)?;
    drop(vault);
    fs::remove_file(root.join("catalog.sqlite3"))?;
    let mut restored = Vault::open(&root)?;
    assert_eq!(restored.rebuild()?, 2);
    for (id, expected) in [(a, &raw), (b, &encoded)] {
        let mut output = Vec::new();
        restored.export(id, &mut output)?;
        assert_eq!(&output, expected);
    }
    Ok(())
}

#[test]
fn unknown_native_encoder_and_malformed_container_remain_opaque() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let raw = content();
    let checksum = native(&raw, true);
    let malformed = b"not a valid zstd\x00\xff".to_vec();
    let mut vault = Vault::init(&dir.path().join("vault"), 3)?;
    for (i, bytes) in [checksum, malformed].into_iter().enumerate() {
        let path = dir.path().join(format!("{i}.jsonl.zst"));
        fs::write(&path, &bytes)?;
        let id = vault
            .ingest(&[candidate(&path)], Source::Auto)?
            .snapshots
            .remove(0);
        assert_eq!(vault.manifest(&id)?.version, 1);
        let mut output = Vec::new();
        vault.export(&id, &mut output)?;
        assert_eq!(output, bytes);
    }
    Ok(())
}

#[test]
fn unchanged_legacy_capture_does_not_write_orphaned_transforms() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("legacy.jsonl");
    let raw = b"unchanged legacy bytes\n".repeat(300);
    fs::write(&path, &raw)?;
    let mut vault = Vault::init(&dir.path().join("vault"), 3)?;
    let mut builder = PackBuilder::new(3);
    builder.add(&raw)?;
    let pack = builder.finish()?;
    vault.receive_object("packs", &pack.id, &pack.bytes)?;
    let manifest = Manifest {
        version: 1,
        revision: 1,
        device_id: vault.config.device_id.clone(),
        origin: fs::canonicalize(&path)?.to_str().unwrap().into(),
        captured_at: "2026-10-01T00:00:00Z".into(),
        content_hash: blake3::hash(&raw).to_hex().to_string(),
        bytes: raw.len() as u64,
        metadata: Metadata::default(),
        chunks: vec![ChunkRef {
            hash: blake3::hash(&raw).to_hex().to_string(),
            pack: pack.id,
            offset: 0,
            len: raw.len() as u32,
        }],
        recipe: None,
    };
    let (id, bytes) = manifest_bytes(&manifest);
    vault.receive_object("manifests", &id, &bytes)?;
    let before = vault.stats()?.archive_bytes;
    let report = vault.ingest(&[candidate(&path)], Source::Auto)?;
    assert_eq!(report.unchanged, 1);
    assert_eq!(report.new_pack_bytes, 0);
    assert_eq!(report.new_chunk_bytes, 0);
    assert_eq!(before, vault.stats()?.archive_bytes);
    assert_eq!(vault.verify()?.orphan_packs, 0);
    Ok(())
}

#[test]
fn malformed_recipes_never_publish_and_existing_snapshots_still_restore() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("input.jsonl");
    let raw = content();
    fs::write(&path, &raw)?;
    let mut vault = Vault::init(&dir.path().join("vault"), 3)?;
    let id = vault
        .ingest(&[candidate(&path)], Source::Auto)?
        .snapshots
        .remove(0);
    let good = vault.manifest(&id)?;
    for case in 0..9 {
        let mut bad = good.clone();
        let r = bad.recipe.as_mut().unwrap();
        match case {
            0 => r.operations[0][0] = u32::MAX,
            1 => r.operations[0][1] = u32::MAX,
            2 => r.operations[0][2] = u32::MAX,
            3 => r.operations[0][3] = u32::MAX,
            4 => r.container = Some("future-unknown-codec".into()),
            5 => {
                r.decoded_hash = "0".repeat(64);
                bad.content_hash = r.decoded_hash.clone();
            }
            6 => {
                bad.bytes = 1;
                r.decoded_bytes = 1;
            }
            7 => bad.chunks.push(bad.chunks[0].clone()),
            8 => bad.version = 1,
            _ => unreachable!(),
        }
        let (bad_id, bytes) = manifest_bytes(&bad);
        assert!(
            vault.receive_object("manifests", &bad_id, &bytes).is_err(),
            "case {case}"
        );
        assert!(!vault
            .root
            .join("manifests")
            .join(format!("{bad_id}.json.zst"))
            .exists());
    }
    assert_eq!(vault.list(None, 10, true)?.len(), 1);
    let mut output = Vec::new();
    vault.export(&id, &mut output)?;
    assert_eq!(output, raw);
    Ok(())
}
