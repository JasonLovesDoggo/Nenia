use nenia::{
    adapters::{Candidate, Source},
    vault::Vault,
};
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn cli(home: &Path, vault: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nenia"));
    command
        .arg("--vault")
        .arg(vault)
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("TOKIO_WORKER_THREADS", "2");
    for key in [
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "PI_CODING_AGENT_DIR",
        "PI_CODING_AGENT_SESSION_DIR",
        "NENIA_VAULT",
        "NENIA_REMOTE",
        "NENIA_TOKEN",
    ] {
        command.env_remove(key);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn native_compressed_traces_are_discovered_searchable_and_restored_as_original_containers() {
    let dir = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let home = dir.path().join("home");
    let root = dir.path().join("vault");
    let sources = home.join(".codex/archived_sessions");
    fs::create_dir_all(&sources).unwrap();
    let raw = concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"native-zstd-session\",\"cwd\":\"/fiction\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"text\":\"Native trace archive\"}]}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"role\":\"assistant\",\"content\":[{\"text\":\"nephelinebodytoken exists only inside compressed content\"}]}}\n"
    );
    let encoded = zstd::encode_all(raw.as_bytes(), 3).unwrap();
    let malformed = b"original malformed zstd container\xff\x00";
    fs::write(sources.join("valid.jsonl.zst"), &encoded).unwrap();
    fs::write(sources.join("malformed.jsonl.zst"), malformed).unwrap();
    let discovered: Value =
        serde_json::from_slice(&cli(&home, &root, &["discover"]).stdout).unwrap();
    assert_eq!(discovered.as_array().unwrap().len(), 2);
    cli(&home, &root, &["ingest", "--index"]);
    let snapshots: Value =
        serde_json::from_slice(&cli(&home, &root, &["--json", "list"]).stdout).unwrap();
    assert_eq!(snapshots.as_array().unwrap().len(), 2);
    for row in snapshots.as_array().unwrap() {
        let id = row["id"].as_str().unwrap();
        let manifest: Value =
            serde_json::from_slice(&cli(&home, &root, &["show", id]).stdout).unwrap();
        let expected = if row["origin"]
            .as_str()
            .unwrap()
            .ends_with("malformed.jsonl.zst")
        {
            assert!(!manifest["metadata"]["warnings"]
                .as_array()
                .unwrap()
                .is_empty());
            malformed.as_slice()
        } else {
            assert_eq!(manifest["metadata"]["session_id"], "native-zstd-session");
            assert_eq!(manifest["metadata"]["title"], "Native trace archive");
            encoded.as_slice()
        };
        assert_eq!(manifest["bytes"].as_u64().unwrap(), expected.len() as u64);
        assert_eq!(
            manifest["content_hash"],
            blake3::hash(expected).to_hex().to_string()
        );
        assert_eq!(cli(&home, &root, &["cat", id]).stdout, expected);
        let output = dir.path().join(format!("{id}.jsonl.zst"));
        cli(
            &home,
            &root,
            &["restore", id, "--output", output.to_str().unwrap()],
        );
        assert_eq!(fs::read(output).unwrap(), expected);
    }
    let found: Value = serde_json::from_slice(
        &cli(&home, &root, &["--json", "search", "nephelinebodytoken"]).stdout,
    )
    .unwrap();
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["session_id"], "native-zstd-session");
    assert_eq!(fs::read(sources.join("valid.jsonl.zst")).unwrap(), encoded);
    cli(&home, &root, &["verify"]);
}

#[test]
fn corrupt_archive_packs_are_not_hidden_by_best_effort_zstd_source_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("trace.jsonl.zst");
    fs::write(
        &source,
        zstd::encode_all(b"{\"text\":\"compressed source\"}\n".as_slice(), 1).unwrap(),
    )
    .unwrap();
    let mut vault = Vault::init(&dir.path().join("vault"), 1).unwrap();
    let id = vault
        .ingest(
            &[Candidate {
                path: source,
                source: Source::Codex,
            }],
            Source::Auto,
        )
        .unwrap()
        .snapshots[0]
        .clone();
    let manifest = vault.manifest(&id).unwrap();
    let path = vault
        .root
        .join("packs")
        .join(format!("{}.nz", manifest.chunks[0].pack));
    let mut corrupted = fs::read(&path).unwrap();
    corrupted[0] ^= 1;
    fs::write(&path, corrupted).unwrap();
    assert!(vault.index(1024).is_err());
    assert!(!vault.list(None, 1, false).unwrap()[0].indexed);
}
