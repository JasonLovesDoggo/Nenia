use nenia::{
    adapters::{Candidate, Source},
    pack::{PackBuilder, MAX_CHUNK_SIZE, MAX_PACK_ENCODED_SIZE, MAX_PACK_RAW_SIZE},
    server,
    vault::{InventoryPage, Vault},
};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path, time::Duration};
use tokio::{net::TcpListener, task::JoinHandle};
use tower::ServiceExt;

const TOKEN: &str = "synthetic-server-test-token-0123456789";

struct Server {
    address: std::net::SocketAddr,
    client: Client,
    task: JoinHandle<()>,
}

impl Server {
    async fn start(vault: Vault, index: bool) -> Self {
        let router = server::router_with_index(vault, TOKEN.into(), index).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            address,
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap(),
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn page(&self, query: &str) -> InventoryPage {
        self.client
            .get(self.url(&format!("/v1/objects{query}")))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn put(&self, kind: &str, id: &str, body: Vec<u8>) -> StatusCode {
        self.client
            .put(self.url(&format!("/v1/{kind}/{id}")))
            .bearer_auth(TOKEN)
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn upload(&self, vault: &Vault, id: &str) {
        let manifest = vault.manifest(id).unwrap();
        let packs: BTreeSet<_> = manifest
            .chunks
            .iter()
            .map(|chunk| chunk.pack.clone())
            .collect();
        for pack in packs {
            assert!(matches!(
                self.put("packs", &pack, vault.object("packs", &pack).unwrap())
                    .await,
                StatusCode::CREATED | StatusCode::NO_CONTENT
            ));
        }
        assert_eq!(
            self.put("manifests", id, vault.object("manifests", id).unwrap())
                .await,
            StatusCode::CREATED
        );
    }

    async fn search(&self, query: &str) -> Vec<Value> {
        self.client
            .get(self.url("/v1/search"))
            .bearer_auth(TOKEN)
            .query(&[("q", query)])
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn keys(page: &InventoryPage) -> BTreeSet<String> {
    page.packs
        .iter()
        .map(|id| format!("packs/{id}"))
        .chain(page.manifests.iter().map(|id| format!("manifests/{id}")))
        .collect()
}

fn capture(vault: &mut Vault, source: &Path, bytes: &[u8]) -> String {
    fs::write(source, bytes).unwrap();
    vault
        .ingest(
            &[Candidate {
                path: source.into(),
                source: Source::Generic,
            }],
            Source::Generic,
        )
        .unwrap()
        .snapshots[0]
        .clone()
}

#[tokio::test]
async fn paged_inventory_keeps_one_complete_snapshot_while_new_uploads_arrive() {
    let directory = tempfile::tempdir().unwrap();
    let mut client_vault = Vault::init(&directory.path().join("client"), 1).unwrap();
    let server = Server::start(
        Vault::init(&directory.path().join("server"), 1).unwrap(),
        false,
    )
    .await;
    let mut ids = Vec::new();
    for number in 0..3 {
        let id = capture(
            &mut client_vault,
            &directory.path().join(format!("trace-{number}")),
            format!("different synthetic trace number {number}\n").as_bytes(),
        );
        server.upload(&client_vault, &id).await;
        ids.push(id);
    }
    let complete = server.page("?limit=100").await;
    let expected = keys(&complete);
    assert_eq!(expected.len(), 6);
    let first = server.page("?limit=2").await;
    let generation = first.generation.clone();
    let watermark = first.watermark;
    let mut observed = keys(&first);
    let mut cursor = first.next_after;
    assert!(cursor.is_some());

    let late = capture(
        &mut client_vault,
        &directory.path().join("late-trace"),
        b"arrived after inventory started\n",
    );
    server.upload(&client_vault, &late).await;
    let mut pages = 1;
    while let Some(after) = cursor {
        let page = server
            .page(&format!("?limit=2&after={after}&watermark={watermark}"))
            .await;
        assert_eq!(page.generation, generation);
        assert_eq!(page.watermark, watermark);
        assert!(page.packs.len() + page.manifests.len() <= 2);
        assert!(page.next_after.is_none_or(|next| next > after));
        for key in keys(&page) {
            assert!(observed.insert(key), "duplicate inventory object");
        }
        cursor = page.next_after;
        pages += 1;
        assert!(
            pages <= expected.len(),
            "inventory pagination did not terminate"
        );
    }
    assert_eq!(observed, expected);
    assert!(!observed.contains(&format!("manifests/{late}")));
    for id in ids {
        for chunk in client_vault.manifest(&id).unwrap().chunks {
            assert!(observed.contains(&format!("packs/{}", chunk.pack)));
        }
    }
    let fresh = server.page("?limit=100").await;
    assert_eq!(fresh.generation, generation);
    assert!(fresh.watermark > watermark);
    assert_eq!(keys(&fresh).len(), expected.len() + 2);
}

#[test]
fn catalog_rebuild_changes_inventory_generation_without_losing_objects() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("vault");
    let mut vault = Vault::init(&root, 1).unwrap();
    capture(
        &mut vault,
        &directory.path().join("trace"),
        b"durable inventory\n",
    );
    let original = vault.inventory_page(0, None, 100).unwrap();
    drop(vault);
    let mut vault = Vault::open(&root).unwrap();
    assert_eq!(
        vault.inventory_page(0, None, 100).unwrap().generation,
        original.generation
    );
    vault.rebuild().unwrap();
    let rebuilt = vault.inventory_page(0, None, 100).unwrap();
    assert_ne!(rebuilt.generation, original.generation);
    assert_eq!(keys(&rebuilt), keys(&original));
    drop(vault);
    Vault::rebuild_catalog(&root).unwrap();
    let vault = Vault::open(&root).unwrap();
    let replaced = vault.inventory_page(0, None, 100).unwrap();
    assert_ne!(replaced.generation, rebuilt.generation);
    assert_eq!(keys(&replaced), keys(&original));
}

#[tokio::test]
async fn uploaded_content_is_searchable_only_when_server_indexing_is_enabled() {
    let directory = tempfile::tempdir().unwrap();
    let mut client_vault = Vault::init(&directory.path().join("client"), 1).unwrap();
    let id = capture(&mut client_vault, &directory.path().join("trace.jsonl"), concat!(
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Ordinary metadata title\"}}\n",
        "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"obsidianlighthouse lives only in the transcript\"}}\n"
    ).as_bytes());
    assert!(!client_vault
        .manifest(&id)
        .unwrap()
        .metadata
        .title
        .unwrap()
        .contains("obsidianlighthouse"));
    for enabled in [false, true] {
        let server = Server::start(
            Vault::init(&directory.path().join(format!("server-{enabled}")), 1).unwrap(),
            enabled,
        )
        .await;
        server.upload(&client_vault, &id).await;
        let found = server.search("obsidianlighthouse").await;
        assert_eq!(found.len(), usize::from(enabled));
        if enabled {
            assert_eq!(found[0]["id"], id);
            assert_eq!(found[0]["indexed"], true);
        }
        assert_eq!(
            server
                .put(
                    "manifests",
                    &id,
                    client_vault.object("manifests", &id).unwrap()
                )
                .await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            server.search("obsidianlighthouse").await.len(),
            usize::from(enabled)
        );
    }
}

#[tokio::test]
async fn rejected_requests_do_not_publish_objects_and_pack_dependencies_are_required() {
    let directory = tempfile::tempdir().unwrap();
    let mut client_vault = Vault::init(&directory.path().join("client"), 1).unwrap();
    let id = capture(
        &mut client_vault,
        &directory.path().join("trace"),
        b"valid tiny archive\n",
    );
    let server = Server::start(
        Vault::init(&directory.path().join("server"), 1).unwrap(),
        false,
    )
    .await;
    assert_eq!(
        server
            .client
            .get(server.url("/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    for token in [None, Some("wrong-token")] {
        let mut request = server.client.get(server.url("/v1/objects"));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    for route in [
        "/v1/unknown/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "/v1/packs/not-a-hash",
    ] {
        assert_eq!(
            server
                .client
                .get(server.url(route))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        server.put("unknown", &id, Vec::new()).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server.put("packs", "not-a-hash", Vec::new()).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .put("packs", &id, b"invalid pack bytes".to_vec())
            .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .put("packs", &id, vec![0; MAX_PACK_ENCODED_SIZE + 1])
            .await,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        server
            .put(
                "manifests",
                &id,
                client_vault.object("manifests", &id).unwrap()
            )
            .await,
        StatusCode::BAD_REQUEST
    );
    assert!(keys(&server.page("").await).is_empty());
    for query in ["?limit=0", "?limit=10001", "?after=-1", "?watermark=1"] {
        assert_eq!(
            server
                .client
                .get(server.url(&format!("/v1/objects{query}")))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    server.upload(&client_vault, &id).await;
    let response = server
        .client
        .get(server.url(&format!("/v1/manifests/{id}")))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        client_vault.object("manifests", &id).unwrap()
    );
}

#[tokio::test]
async fn retained_download_bodies_hold_capacity_until_the_last_byte_owner_is_dropped() {
    let directory = tempfile::tempdir().unwrap();
    let mut vault = Vault::init(&directory.path().join("server"), 1).unwrap();
    let mut builder = PackBuilder::new(1);
    let mut state = 0x8472_1305_4928_3529u64;
    for _ in 0..MAX_PACK_RAW_SIZE / MAX_CHUNK_SIZE {
        let mut chunk = vec![0; MAX_CHUNK_SIZE];
        for bytes in chunk.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            bytes.copy_from_slice(&state.to_le_bytes());
        }
        builder.add(&chunk).unwrap();
    }
    let pack = builder.finish().unwrap();
    assert!(pack.bytes.len() > 7 * 1024 * 1024);
    vault
        .receive_object("packs", &pack.id, &pack.bytes)
        .unwrap();
    let router = server::router(vault, TOKEN.into()).unwrap();
    let route = format!("/v1/packs/{}", pack.id);
    let request = || {
        axum::http::Request::builder()
            .uri(&route)
            .header("Authorization", format!("Bearer {TOKEN}"))
            .body(axum::body::Body::empty())
            .unwrap()
    };
    // Hold bodies before any socket can copy them into its OS-specific buffers.
    let mut retained = Vec::new();
    for _ in 0..8 {
        let response = router.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        retained.push(response);
    }
    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let bytes = axum::body::to_bytes(retained.pop().unwrap().into_body(), MAX_PACK_ENCODED_SIZE)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), pack.bytes);
    let another_owner = bytes.clone();
    drop(bytes);
    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(another_owner);
    let response = router.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), MAX_PACK_ENCODED_SIZE)
            .await
            .unwrap()
            .as_ref(),
        pack.bytes
    );
}

#[cfg(unix)]
#[test]
fn wrapper_survives_its_own_sigint_and_backs_up_a_signalled_child() {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let directory = tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
    let home = directory.path().join("home");
    let spool = directory.path().join("spool");
    let remote = directory.path().join("remote");
    let source = directory.path().join("trace.jsonl");
    let ready = directory.path().join("child-ready");
    let release = directory.path().join("release-child");
    fs::create_dir(&home).unwrap();
    fs::write(&source, b"preserve trace after Ctrl-C\n").unwrap();
    let remote_url = url::Url::from_directory_path(&remote).unwrap().to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_nenia"));
    command.arg("--vault").arg(&spool).arg("run").arg("--path").arg(&source)
        .arg("--remote").arg(remote_url).args(["--", "sh", "-c"])
        // The child signals only itself. Its bounded wait cannot leave an orphan loop.
        .arg("printf ready > \"$1\"; i=0; while [ ! -f \"$2\" ] && [ \"$i\" -lt 5 ]; do sleep 1; i=$((i+1)); done; kill -INT $$; exit 99")
        .arg("synthetic-signal-child").arg(&ready).arg(&release)
        .env("HOME", &home).env("USERPROFILE", &home).env("TOKIO_WORKER_THREADS", "2")
        .env_remove("NENIA_VAULT").env_remove("NENIA_REMOTE").env_remove("NENIA_TOKEN")
        .stdout(Stdio::null()).stderr(Stdio::null())
        .process_group(0); // An isolated group; the real test runner's group is never signalled.
    let mut process = ChildGuard(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "wrapper exited before the child started"
        );
        assert!(Instant::now() < deadline, "child did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Give the wrapper time to install its asynchronous Ctrl-C listener.
    std::thread::sleep(Duration::from_millis(100));
    assert!(process.0.try_wait().unwrap().is_none());
    assert!(Command::new("/bin/kill")
        .args(["-s", "INT", &process.0.id().to_string()])
        .status()
        .unwrap()
        .success()); // Target only our known wrapper PID, never a group.
    fs::write(release, b"release").unwrap();
    let status = loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "wrapper did not finish after child SIGINT"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(130));
    assert_eq!(fs::read_dir(remote.join("manifests")).unwrap().count(), 1);
    assert_eq!(fs::read(&source).unwrap(), b"preserve trace after Ctrl-C\n");
}
