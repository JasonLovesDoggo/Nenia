use nenia::adapters::{Candidate, Source};
use nenia::remote::{self, SyncOptions};
use nenia::vault::Vault;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn candidate(path: &Path) -> Candidate {
    Candidate {
        path: path.to_owned(),
        source: Source::Generic,
    }
}

#[test]
fn missing_cached_pack_cannot_publish_an_unrestorable_new_snapshot() {
    let directory = tempdir().unwrap();
    let mut vault = Vault::init(&directory.path().join("vault"), 1).unwrap();
    let first = directory.path().join("first.txt");
    let second = directory.path().join("second.txt");
    fs::write(&first, b"the same original source bytes").unwrap();
    fs::copy(&first, &second).unwrap();
    let report = vault.ingest(&[candidate(&first)], Source::Generic).unwrap();
    let manifest = vault.manifest(&report.snapshots[0]).unwrap();
    fs::remove_file(
        vault
            .root
            .join(format!("packs/{}.nz", manifest.chunks[0].pack)),
    )
    .unwrap();
    let error = vault
        .ingest(&[candidate(&second)], Source::Generic)
        .unwrap_err();
    assert!(format!("{error:#}").contains("missing pack"));
    assert_eq!(vault.list(None, 100, true).unwrap().len(), 1);
    assert_eq!(
        fs::read_dir(vault.root.join("manifests")).unwrap().count(),
        1
    );
}

#[test]
fn a_missing_manifest_cannot_be_reported_as_an_unchanged_successful_capture() {
    for fast in [false, true] {
        let directory = tempdir().unwrap();
        let mut vault = Vault::init(&directory.path().join("vault"), 1).unwrap();
        let source = directory.path().join("trace.txt");
        fs::write(&source, b"recoverable bytes still present in source").unwrap();
        let report = vault
            .ingest(&[candidate(&source)], Source::Generic)
            .unwrap();
        fs::remove_file(
            vault
                .root
                .join(format!("manifests/{}.json.zst", report.snapshots[0])),
        )
        .unwrap();
        let result = if fast {
            vault.ingest_changed(&[candidate(&source)], Source::Generic)
        } else {
            vault.ingest(&[candidate(&source)], Source::Generic)
        };
        assert!(
            result.is_err(),
            "capture reported success despite missing manifest (fast={fast})"
        );
    }
}

#[test]
fn capture_revisions_keep_latest_correct_after_clock_rollback_and_rebuild() {
    let directory = tempdir().unwrap();
    let mut vault = Vault::init(&directory.path().join("vault"), 1).unwrap();
    let source = directory.path().join("clock.txt");
    fs::write(&source, b"first capture").unwrap();
    let first = vault
        .ingest(&[candidate(&source)], Source::Generic)
        .unwrap();
    fs::write(&source, b"second capture is current").unwrap();
    let second = vault
        .ingest(&[candidate(&source)], Source::Generic)
        .unwrap();
    let mut older = vault.manifest(&first.snapshots[0]).unwrap();
    assert_eq!(older.revision, 1);
    assert_eq!(vault.manifest(&second.snapshots[0]).unwrap().revision, 2);
    older.captured_at = "2099-01-01T00:00:00+00:00".into();
    let encoded = zstd::encode_all(serde_json::to_vec(&older).unwrap().as_slice(), 1).unwrap();
    let id = blake3::hash(&encoded).to_hex().to_string();
    vault.receive_object("manifests", &id, &encoded).unwrap();
    for rebuild in [false, true] {
        if rebuild {
            vault.rebuild().unwrap();
        }
        let latest = vault.list(None, 10, false).unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].id, second.snapshots[0]);
        let unchanged = vault
            .ingest(&[candidate(&source)], Source::Generic)
            .unwrap();
        assert_eq!(unchanged.unchanged, 1);
        assert_eq!(unchanged.archived, 0);
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_paths_fail_explicitly_instead_of_aliasing_source_identity() {
    use std::os::unix::ffi::OsStringExt;
    for fast in [false, true] {
        let directory = tempdir().unwrap();
        let mut vault = Vault::init(&directory.path().join("vault"), 1).unwrap();
        let source = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"trace-\xff.txt".to_vec()));
        fs::write(&source, b"preserve identity as well as bytes").unwrap();
        let result = if fast {
            vault.ingest_changed(&[candidate(&source)], Source::Generic)
        } else {
            vault.ingest(&[candidate(&source)], Source::Generic)
        };
        let error = result.unwrap_err();
        assert!(format!("{error:#}").contains("UTF-8"));
        assert!(vault.list(None, 10, true).unwrap().is_empty());
    }
}

#[test]
fn restore_never_publishes_partial_output_when_a_later_pack_is_corrupt() {
    let directory = tempdir().unwrap();
    let output_directory = tempdir().unwrap();
    let mut vault = Vault::init(&directory.path().join("vault"), 1).unwrap();
    let source = directory.path().join("large.bin");
    let mut random = 0x9876_1234_abcd_5555_u64;
    let bytes: Vec<u8> = (0..9 * 1024 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random as u8
        })
        .collect();
    fs::write(&source, &bytes).unwrap();
    let report = vault
        .ingest(&[candidate(&source)], Source::Generic)
        .unwrap();
    let id = &report.snapshots[0];
    let manifest = vault.manifest(id).unwrap();
    let first_pack = &manifest.chunks[0].pack;
    let last_pack = &manifest.chunks.last().unwrap().pack;
    assert_ne!(first_pack, last_pack);
    let path = vault.root.join(format!("packs/{last_pack}.nz"));
    let mut corrupt = fs::read(&path).unwrap();
    corrupt[0] ^= 1;
    fs::write(path, corrupt).unwrap();
    let output = output_directory.path().join("restored.bin");
    assert!(vault.export_file(id, &output).is_err());
    assert!(!output.exists());
    assert_eq!(fs::read_dir(output_directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn two_devices_with_the_same_source_path_keep_separate_searchable_histories() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("shared-name.txt");
    fs::write(&source, b"obsidianquasar exists on both devices\n").unwrap();
    let mut first = Vault::init(&directory.path().join("device-a"), 1).unwrap();
    let mut second = Vault::init(&directory.path().join("device-b"), 1).unwrap();
    first
        .ingest(&[candidate(&source)], Source::Generic)
        .unwrap();
    second
        .ingest(&[candidate(&source)], Source::Generic)
        .unwrap();
    assert_ne!(first.config.device_id, second.config.device_id);
    let server_vault = Vault::init(&directory.path().join("server"), 1).unwrap();
    let token = "reliability-test-bearer-token-123";
    let app = nenia::server::router_with_index(server_vault, token.into(), true).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let options = SyncOptions {
        token: Some(token.into()),
        ..SyncOptions::default()
    };
    // Spawning the client future also guards its Send requirement for server mirrors.
    let upload_root = first.root.clone();
    let upload_address = address.clone();
    let upload_options = options.clone();
    tokio::spawn(async move { remote::push(&upload_root, &upload_address, &upload_options).await })
        .await
        .unwrap()
        .unwrap();
    remote::push(&second.root, &address, &options)
        .await
        .unwrap();
    let client = reqwest::Client::new();
    let snapshots: Vec<serde_json::Value> = client
        .get(format!("{address}/v1/snapshots"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshots.len(), 2);
    assert_ne!(snapshots[0]["device_id"], snapshots[1]["device_id"]);
    let results: Vec<serde_json::Value> = client
        .get(format!("{address}/v1/search?q=obsidianquasar"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    task.abort();
}

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_running_server_mirrors_a_complete_recoverable_archive() {
    let directory = tempdir().unwrap();
    let mirror_directory = tempdir().unwrap();
    let server_root = directory.path().join("server");
    let source = directory.path().join("trace.txt");
    let bytes = b"server-owned replication keeps S3 credentials off clients\n";
    fs::write(&source, bytes).unwrap();
    let mut vault = Vault::init(&server_root, 1).unwrap();
    let report = vault
        .ingest(&[candidate(&source)], Source::Generic)
        .unwrap();
    let snapshot = report.snapshots[0].clone();
    drop(vault);
    let remote = url::Url::from_directory_path(mirror_directory.path())
        .unwrap()
        .to_string();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_nenia"))
        .arg("--vault")
        .arg(&server_root)
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--mirror",
            &remote,
            "--mirror-interval",
            "10",
        ])
        .env("NENIA_TOKEN", "mirror-reliability-token-at-least-24")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let manifest_path = mirror_directory
        .path()
        .join(format!("manifests/{snapshot}.json.zst"));
    while !manifest_path.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "server exited before mirroring"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "initial mirror did not complete"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The server still owns its lock when the external copy becomes independently recoverable.
    assert!(Vault::open(&server_root).is_err());
    let mut mirrored = Vault::init(mirror_directory.path(), 1).unwrap();
    assert_eq!(mirrored.rebuild().unwrap(), 1);
    let mut restored = Vec::new();
    mirrored.export(&snapshot, &mut restored).unwrap();
    assert_eq!(restored, bytes);
}
