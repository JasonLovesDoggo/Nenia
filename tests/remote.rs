use nenia::adapters::Metadata;
use nenia::pack::{ChunkRef, PackBuilder};
use nenia::remote::{pull, push, SyncOptions};
use nenia::vault::Manifest;
use std::fs;
use std::path::Path;
use tempfile::{tempdir, TempDir};

struct Fixture {
    directory: TempDir,
    pack: String,
    manifest: String,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("packs")).unwrap();
        fs::create_dir(directory.path().join("manifests")).unwrap();
        let content = b"{\"role\":\"user\",\"content\":\"preserve this exact trace\"}\n";
        let mut builder = PackBuilder::new(3);
        builder.add(content).unwrap();
        let pack = builder.finish().unwrap();
        fs::write(
            directory.path().join(format!("packs/{}.nz", pack.id)),
            &pack.bytes,
        )
        .unwrap();
        let manifest = Manifest {
            version: 1,
            device_id: "test-device".into(),
            origin: "laptop:session.jsonl".into(),
            captured_at: "2026-10-27T21:00:00Z".into(),
            content_hash: blake3::hash(content).to_hex().to_string(),
            bytes: content.len() as u64,
            metadata: Metadata::default(),
            chunks: vec![ChunkRef {
                hash: blake3::hash(content).to_hex().to_string(),
                pack: pack.id.clone(),
                offset: 0,
                len: content.len() as u32,
            }],
        };
        let encoded =
            zstd::encode_all(serde_json::to_vec(&manifest).unwrap().as_slice(), 3).unwrap();
        let id = blake3::hash(&encoded).to_hex().to_string();
        fs::write(
            directory.path().join(format!("manifests/{id}.json.zst")),
            encoded,
        )
        .unwrap();
        Self {
            directory,
            pack: pack.id,
            manifest: id,
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }
    fn pack_key(&self) -> String {
        format!("packs/{}.nz", self.pack)
    }
    fn manifest_key(&self) -> String {
        format!("manifests/{}.json.zst", self.manifest)
    }
}

fn url(path: &Path) -> String {
    url::Url::from_directory_path(path).unwrap().to_string()
}

fn corrupt(path: &Path) {
    let mut bytes = fs::read(path).unwrap();
    bytes[0] ^= 1;
    fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn file_roundtrip_is_additive_exact_and_idempotent() {
    let source = Fixture::new();
    let remote = tempdir().unwrap();
    let destination = tempdir().unwrap();
    fs::write(
        source.root().join("catalog.sqlite3"),
        "not an archive object",
    )
    .unwrap();
    fs::write(source.root().join("config.json"), "local config").unwrap();
    fs::write(source.root().join("packs/interrupted.nz.tmp"), "incomplete").unwrap();
    fs::write(remote.path().join("unrelated"), "keep me").unwrap();
    let options = SyncOptions::default();
    let uploaded = push(source.root(), &url(remote.path()), &options)
        .await
        .unwrap();
    assert_eq!(uploaded.transferred_objects, 2);
    assert!(uploaded.transferred_bytes > 0);
    let repeated = push(source.root(), &url(remote.path()), &options)
        .await
        .unwrap();
    assert_eq!(repeated.transferred_objects, 0);
    assert_eq!(repeated.existing_objects, 2);
    assert_eq!(repeated.verified_existing_objects, 2);
    assert!(!remote.path().join("catalog.sqlite3").exists());
    assert!(!remote.path().join("config.json").exists());
    assert!(!remote.path().join("packs/interrupted.nz.tmp").exists());
    assert_eq!(
        fs::read_to_string(remote.path().join("unrelated")).unwrap(),
        "keep me"
    );
    let downloaded = pull(destination.path(), &url(remote.path()), &options)
        .await
        .unwrap();
    assert_eq!(downloaded.transferred_objects, 2);
    for key in [source.pack_key(), source.manifest_key()] {
        assert_eq!(
            fs::read(source.root().join(&key)).unwrap(),
            fs::read(destination.path().join(key)).unwrap()
        );
    }
    let repeated = pull(destination.path(), &url(remote.path()), &options)
        .await
        .unwrap();
    assert_eq!(repeated.transferred_objects, 0);
    assert_eq!(repeated.verified_existing_objects, 2);
}

#[tokio::test]
async fn push_rejects_corrupted_source_before_publishing_manifests() {
    let source = Fixture::new();
    let remote = tempdir().unwrap();
    corrupt(&source.root().join(source.pack_key()));
    let error = push(source.root(), &url(remote.path()), &SyncOptions::default())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("hash mismatch"));
    assert!(!remote.path().join(source.manifest_key()).exists());
}

#[tokio::test]
async fn push_never_silently_trusts_or_overwrites_corrupted_existing_remote() {
    let source = Fixture::new();
    let remote = tempdir().unwrap();
    push(source.root(), &url(remote.path()), &SyncOptions::default())
        .await
        .unwrap();
    let path = remote.path().join(source.pack_key());
    corrupt(&path);
    let before = fs::read(&path).unwrap();
    let error = push(source.root(), &url(remote.path()), &SyncOptions::default())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("hash mismatch"));
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn explicit_trust_option_reports_unverified_existing_objects() {
    let source = Fixture::new();
    let remote = tempdir().unwrap();
    push(source.root(), &url(remote.path()), &SyncOptions::default())
        .await
        .unwrap();
    corrupt(&remote.path().join(source.pack_key()));
    let options = SyncOptions {
        verify_existing: false,
        ..SyncOptions::default()
    };
    let report = push(source.root(), &url(remote.path()), &options)
        .await
        .unwrap();
    assert_eq!(report.existing_objects, 2);
    assert_eq!(report.verified_existing_objects, 0);
}

#[tokio::test]
async fn trusting_existing_remote_avoids_reading_old_local_payloads() {
    let source = Fixture::new();
    let remote = tempdir().unwrap();
    push(source.root(), &url(remote.path()), &SyncOptions::default())
        .await
        .unwrap();
    corrupt(&source.root().join(source.pack_key()));
    corrupt(&source.root().join(source.manifest_key()));
    let options = SyncOptions {
        verify_existing: false,
        ..SyncOptions::default()
    };
    let skipped = push(source.root(), &url(remote.path()), &options)
        .await
        .unwrap();
    assert_eq!(skipped.existing_objects, 2);
    assert_eq!(skipped.verified_existing_objects, 0);
    assert!(
        push(source.root(), &url(remote.path()), &SyncOptions::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn trust_option_still_rejects_size_mismatch() {
    let source = Fixture::new();
    let remote = tempdir().unwrap();
    push(source.root(), &url(remote.path()), &SyncOptions::default())
        .await
        .unwrap();
    fs::write(remote.path().join(source.pack_key()), b"truncated").unwrap();
    let options = SyncOptions {
        verify_existing: false,
        ..SyncOptions::default()
    };
    assert!(push(source.root(), &url(remote.path()), &options)
        .await
        .is_err());
}

#[tokio::test]
async fn pull_rejects_corrupted_source_before_publishing_manifest() {
    let remote = Fixture::new();
    let destination = tempdir().unwrap();
    corrupt(&remote.root().join(remote.pack_key()));
    let error = pull(
        destination.path(),
        &url(remote.root()),
        &SyncOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("hash mismatch"));
    assert!(!destination.path().join(remote.manifest_key()).exists());
    assert!(!destination.path().join(remote.pack_key()).exists());
}

#[tokio::test]
async fn pull_rejects_corrupted_existing_local_object() {
    let remote = Fixture::new();
    let destination = tempdir().unwrap();
    pull(
        destination.path(),
        &url(remote.root()),
        &SyncOptions::default(),
    )
    .await
    .unwrap();
    let path = destination.path().join(remote.pack_key());
    corrupt(&path);
    let before = fs::read(&path).unwrap();
    assert!(pull(
        destination.path(),
        &url(remote.root()),
        &SyncOptions::default()
    )
    .await
    .is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn neither_direction_publishes_manifests_with_missing_packs() {
    let incomplete = Fixture::new();
    fs::remove_file(incomplete.root().join(incomplete.pack_key())).unwrap();
    let push_destination = tempdir().unwrap();
    let pull_destination = tempdir().unwrap();
    let error = push(
        incomplete.root(),
        &url(push_destination.path()),
        &SyncOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("missing pack"));
    let error = pull(
        pull_destination.path(),
        &url(incomplete.root()),
        &SyncOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("missing pack"));
    assert!(!push_destination
        .path()
        .join(incomplete.manifest_key())
        .exists());
    assert!(!pull_destination
        .path()
        .join(incomplete.manifest_key())
        .exists());
}

#[tokio::test]
async fn malformed_manifest_with_correct_object_hash_is_rejected() {
    let source = Fixture::new();
    fs::remove_file(source.root().join(source.manifest_key())).unwrap();
    let bytes = b"not a zstd manifest";
    let key = format!("manifests/{}.json.zst", blake3::hash(bytes).to_hex());
    fs::write(source.root().join(&key), bytes).unwrap();
    let remote = tempdir().unwrap();
    assert!(
        push(source.root(), &url(remote.path()), &SyncOptions::default())
            .await
            .is_err()
    );
    assert!(!remote.path().join(key).exists());
}

#[tokio::test]
async fn oversized_sparse_pack_is_rejected_without_loading_it() {
    let source = Fixture::new();
    fs::OpenOptions::new()
        .write(true)
        .open(source.root().join(source.pack_key()))
        .unwrap()
        .set_len(nenia::pack::MAX_PACK_ENCODED_SIZE as u64 + 1)
        .unwrap();
    let remote = tempdir().unwrap();
    let error = push(source.root(), &url(remote.path()), &SyncOptions::default())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("size limit"));
}

#[tokio::test]
async fn unsafe_urls_and_invalid_concurrency_fail_without_network_access() {
    let source = Fixture::new();
    for remote in [
        "s3://bucket/a/../b",
        "s3://bucket/a/./b",
        "s3://bucket/a/%2e%2e/b",
        "s3://bucket/a//b",
        "s3://user:secret@bucket",
        "s3://bucket?token=secret",
        "ftp://example.org/archive",
        "https://user:secret@example.org",
        "https://example.org/#fragment",
    ] {
        assert!(
            push(source.root(), remote, &SyncOptions::default())
                .await
                .is_err(),
            "accepted {remote}"
        );
    }
    for concurrency in [0, 33] {
        let options = SyncOptions {
            concurrency,
            ..SyncOptions::default()
        };
        assert!(push(source.root(), "https://example.invalid", &options)
            .await
            .is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_objects_and_directories_cannot_escape_archive_root() {
    use std::os::unix::fs::symlink;
    let source = Fixture::new();
    let remote = tempdir().unwrap();
    let outside = tempdir().unwrap();
    symlink(outside.path(), remote.path().join("packs")).unwrap();
    assert!(
        push(source.root(), &url(remote.path()), &SyncOptions::default())
            .await
            .is_err()
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    fs::remove_file(remote.path().join("packs")).unwrap();
    fs::create_dir(remote.path().join("packs")).unwrap();
    let outside_file = outside.path().join("precious");
    fs::write(&outside_file, b"do not touch").unwrap();
    symlink(&outside_file, remote.path().join(source.pack_key())).unwrap();
    assert!(
        push(source.root(), &url(remote.path()), &SyncOptions::default())
            .await
            .is_err()
    );
    assert_eq!(fs::read(outside_file).unwrap(), b"do not touch");
}

mod http {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::{Path as HttpPath, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    type Objects = Arc<Mutex<BTreeMap<(String, String), Vec<u8>>>>;
    const TOKEN: &str = "integration-test-token";

    fn authorized(headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .is_some_and(|value| value == format!("Bearer {TOKEN}").as_str())
    }

    async fn inventory(
        State(objects): State<Objects>,
        headers: HeaderMap,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        if !authorized(&headers) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let objects = objects.lock().unwrap();
        let packs: Vec<_> = objects
            .keys()
            .filter(|(kind, _)| kind == "packs")
            .map(|(_, id)| id)
            .collect();
        let manifests: Vec<_> = objects
            .keys()
            .filter(|(kind, _)| kind == "manifests")
            .map(|(_, id)| id)
            .collect();
        Ok(Json(
            serde_json::json!({ "packs": packs, "manifests": manifests }),
        ))
    }

    async fn get_blob(
        State(objects): State<Objects>,
        HttpPath(key): HttpPath<(String, String)>,
        headers: HeaderMap,
    ) -> Result<Vec<u8>, StatusCode> {
        if !authorized(&headers) {
            return Err(StatusCode::UNAUTHORIZED);
        }
        objects
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)
    }

    async fn put_blob(
        State(objects): State<Objects>,
        HttpPath(key): HttpPath<(String, String)>,
        headers: HeaderMap,
        bytes: Bytes,
    ) -> StatusCode {
        if !authorized(&headers) {
            return StatusCode::UNAUTHORIZED;
        }
        if blake3::hash(&bytes).to_hex().as_str() != key.1 {
            return StatusCode::BAD_REQUEST;
        }
        let mut objects = objects.lock().unwrap();
        if let Some(existing) = objects.get(&key) {
            return if existing == &bytes {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::CONFLICT
            };
        }
        if key.0 == "manifests" {
            let manifest = nenia::vault::decode_manifest(&bytes, &key.1).unwrap();
            assert!(
                manifest
                    .chunks
                    .iter()
                    .all(|chunk| objects.contains_key(&("packs".into(), chunk.pack.clone()))),
                "manifest arrived before its packs"
            );
        }
        objects.insert(key, bytes.to_vec());
        StatusCode::CREATED
    }

    async fn server() -> (String, Objects, tokio::task::JoinHandle<()>) {
        let objects = Objects::default();
        let router = Router::new()
            .route("/prefix/v1/objects", get(inventory))
            .route("/prefix/v1/{kind}/{id}", get(get_blob).put(put_blob))
            .with_state(objects.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/prefix", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (url, objects, task)
    }

    fn options() -> SyncOptions {
        SyncOptions {
            token: Some(TOKEN.into()),
            ..SyncOptions::default()
        }
    }

    #[tokio::test]
    async fn http_auth_prefix_roundtrip_idempotency_and_publication_order() {
        let fixture = Fixture::new();
        let destination = tempdir().unwrap();
        let (remote, _, task) = server().await;
        assert_eq!(
            push(fixture.root(), &remote, &options())
                .await
                .unwrap()
                .transferred_objects,
            2
        );
        let repeat = push(fixture.root(), &remote, &options()).await.unwrap();
        assert_eq!(repeat.transferred_objects, 0);
        assert_eq!(repeat.verified_existing_objects, 2);
        assert_eq!(
            pull(destination.path(), &remote, &options())
                .await
                .unwrap()
                .transferred_objects,
            2
        );
        assert_eq!(
            fs::read(fixture.root().join(fixture.pack_key())).unwrap(),
            fs::read(destination.path().join(fixture.pack_key())).unwrap()
        );
        assert_eq!(
            pull(destination.path(), &remote, &options())
                .await
                .unwrap()
                .verified_existing_objects,
            2
        );
        let wrong_token = SyncOptions {
            token: Some("wrong".into()),
            ..SyncOptions::default()
        };
        let error = push(fixture.root(), &remote, &wrong_token)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("401"));
        task.abort();
    }

    #[tokio::test]
    async fn http_remote_corruption_and_missing_packs_are_rejected() {
        let fixture = Fixture::new();
        let destination = tempdir().unwrap();
        let (remote, objects, task) = server().await;
        push(fixture.root(), &remote, &options()).await.unwrap();
        objects
            .lock()
            .unwrap()
            .get_mut(&("packs".into(), fixture.pack.clone()))
            .unwrap()[0] ^= 1;
        assert!(push(fixture.root(), &remote, &options()).await.is_err());
        assert!(pull(destination.path(), &remote, &options()).await.is_err());
        assert!(!destination.path().join(fixture.manifest_key()).exists());
        objects
            .lock()
            .unwrap()
            .remove(&("packs".into(), fixture.pack.clone()));
        let error = pull(destination.path(), &remote, &options())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("missing pack"));
        task.abort();
    }

    #[tokio::test]
    async fn real_server_accepts_archive_and_restores_exact_bytes() {
        let fixture = Fixture::new();
        let server_directory = tempdir().unwrap();
        let destination = tempdir().unwrap();
        let token = "real-server-integration-token-123";
        let vault = nenia::vault::Vault::init(server_directory.path(), 3).unwrap();
        let router = nenia::server::router(vault, token.into()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let options = SyncOptions {
            token: Some(token.into()),
            ..SyncOptions::default()
        };
        assert_eq!(
            push(fixture.root(), &remote, &options)
                .await
                .unwrap()
                .transferred_objects,
            2
        );
        let mut restored = nenia::vault::Vault::init(destination.path(), 3).unwrap();
        pull(destination.path(), &remote, &options).await.unwrap();
        assert_eq!(restored.rebuild().unwrap(), 1);
        assert_eq!(restored.verify().unwrap().snapshots, 1);
        let mut bytes = Vec::new();
        restored.export(&fixture.manifest, &mut bytes).unwrap();
        assert_eq!(
            bytes,
            b"{\"role\":\"user\",\"content\":\"preserve this exact trace\"}\n"
        );
        task.abort();
    }

    #[tokio::test]
    async fn http_redirects_are_rejected_and_never_followed() {
        let fixture = Fixture::new();
        let router = Router::new().route(
            "/v1/objects",
            get(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [("location", "http://127.0.0.1:9/steal-token")],
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let error = push(fixture.root(), &remote, &options()).await.unwrap_err();
        assert!(format!("{error:#}").contains("HTTP 307"));
        assert!(!format!("{error:#}").contains(TOKEN));
        task.abort();
    }

    #[tokio::test]
    async fn transient_http_inventory_failure_is_retried() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fixture = Fixture::new();
        let requests = Arc::new(AtomicUsize::new(0));
        let handler_requests = requests.clone();
        let router = Router::new().route(
            "/v1/objects",
            get(move || {
                let requests = handler_requests.clone();
                async move {
                    let status = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    };
                    // An empty inventory lets pull finish without requesting object bodies.
                    (
                        status,
                        Json(serde_json::json!({"packs": [], "manifests": []})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        pull(fixture.root(), &remote, &options()).await.unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        task.abort();
    }
}
