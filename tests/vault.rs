use nenia::{
    adapters::{Candidate, Source},
    vault::Vault,
};
use std::fs;

fn candidate(path: &std::path::Path) -> Candidate {
    Candidate {
        path: path.to_owned(),
        source: Source::Generic,
    }
}

#[test]
fn lossless_snapshots_deduplicate_and_recover_without_catalog() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("vault");
    let path = dir.path().join("trace.jsonl");
    let first = b"{\"message\":\"copper dragons remember\"}\r\ninvalid\xff\x00\n";
    fs::write(&path, first)?;
    let mut v = Vault::init(&root, 3)?;
    let report = v.ingest(&[candidate(&path)], Source::Auto)?;
    assert_eq!(report.archived, 1);
    let id = report.snapshots[0].clone();
    let mut restored = Vec::new();
    v.export(&id, &mut restored)?;
    assert_eq!(restored, first);
    let unchanged = v.ingest(&[candidate(&path)], Source::Auto)?;
    assert_eq!(unchanged.unchanged, 1);
    assert_eq!(unchanged.new_pack_bytes, 0);
    let mut appended = first.to_vec();
    appended.extend_from_slice(b"more content\n");
    fs::write(&path, &appended)?;
    v.ingest(&[candidate(&path)], Source::Auto)?;
    assert_eq!(v.list(None, 100, false)?.len(), 1);
    assert_eq!(v.list(None, 100, true)?.len(), 2);
    assert_eq!(v.verify()?.snapshots, 2);
    let stats = v.stats()?;
    assert!(stats.catalog_bytes > 0);
    assert!(stats.manifest_bytes > 0);
    drop(v);
    for name in [
        "catalog.sqlite3",
        "catalog.sqlite3-wal",
        "catalog.sqlite3-shm",
    ] {
        let p = root.join(name);
        if p.exists() {
            fs::remove_file(p)?;
        }
    }
    let mut recovered = Vault::open(&root)?;
    assert_eq!(recovered.rebuild()?, 2);
    let mut restored = Vec::new();
    recovered.export(&id, &mut restored)?;
    assert_eq!(restored, first);
    Ok(())
}

#[test]
fn index_is_optional_searchable_and_rebuildable() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("trace.jsonl");
    fs::write(&path,b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Archive demo\"}}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"azurite raccoons\"}}\n")?;
    let mut v = Vault::init(&dir.path().join("v"), 3)?;
    v.ingest(&[candidate(&path)], Source::Auto)?;
    assert!(v.search("azurite", 10, false)?.is_empty());
    assert_eq!(v.index(1024 * 1024)?, 1);
    assert_eq!(v.search("azurite raccoons", 10, false)?.len(), 1);
    assert_eq!(v.index(1024)?, 0);
    v.rebuild()?;
    assert!(v.search("azurite", 10, false)?.is_empty());
    v.index(1024)?;
    assert_eq!(v.search("azurite", 10, false)?.len(), 1);
    Ok(())
}

#[test]
fn corruption_and_overwriting_restore_are_rejected() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("binary");
    fs::write(&path, [0, 1, 255, 42])?;
    let mut v = Vault::init(&dir.path().join("v"), 3)?;
    let id = v.ingest(&[candidate(&path)], Source::Auto)?.snapshots[0].clone();
    assert!(v.export_file(&id, &path).is_err());
    let m = v.manifest(&id)?;
    let pack = v
        .root
        .join("packs")
        .join(format!("{}.nz", m.chunks[0].pack));
    fs::write(pack, b"corrupt")?;
    assert!(v.verify().is_err());
    let output = dir.path().join("restored");
    assert!(v.export_file(&id, &output).is_err());
    assert!(!output.exists());
    assert!(v.rebuild().is_err());
    assert_eq!(v.list(None, 10, true)?.len(), 1);
    Ok(())
}

#[test]
fn empty_files_and_exclusive_lock() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("empty");
    fs::write(&path, [])?;
    let mut v = Vault::init(&dir.path().join("v"), 3)?;
    assert!(Vault::open(&v.root).is_err());
    let id = v.ingest(&[candidate(&path)], Source::Auto)?.snapshots[0].clone();
    assert!(v.manifest(&id)?.chunks.is_empty());
    let mut output = Vec::new();
    v.export(&id, &mut output)?;
    assert!(output.is_empty());
    assert_eq!(v.verify()?.snapshots, 1);
    Ok(())
}

#[test]
fn remote_publication_requires_packs_and_preserves_devices() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("trace");
    fs::write(&path, b"same bytes on both devices")?;
    let mut a = Vault::init(&dir.path().join("a"), 3)?;
    let mut b = Vault::init(&dir.path().join("b"), 3)?;
    let mut server = Vault::init(&dir.path().join("server"), 3)?;
    for client in [&mut a, &mut b] {
        let id = client.ingest(&[candidate(&path)], Source::Auto)?.snapshots[0].clone();
        let m = client.manifest(&id)?;
        if server.inventory()?.packs.is_empty() {
            assert!(server
                .receive_object("manifests", &id, &client.object("manifests", &id)?)
                .is_err());
        }
        for c in &m.chunks {
            server.receive_object("packs", &c.pack, &client.object("packs", &c.pack)?)?;
        }
        server.receive_object("manifests", &id, &client.object("manifests", &id)?)?;
    }
    assert_eq!(server.list(None, 10, false)?.len(), 2);
    assert_eq!(server.stats()?.origins, 2);
    assert_eq!(server.inventory()?.packs.len(), 1);
    Ok(())
}
