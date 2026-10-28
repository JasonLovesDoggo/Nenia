//! Real-process coverage with isolated homes. Never discovers the developer's traces.
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const TOKEN: &str = "synthetic-local-test-token-0123456789";

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    vault: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // macOS aliases /tmp and /var; discovery deliberately skips symlinks.
        let directory =
            tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let root = directory.path().to_path_buf();
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        Self {
            _directory: directory,
            vault: root.join("spool"),
            root,
            home,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nenia"));
        command
            .arg("--vault")
            .arg(&self.vault)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join("AppData").join("Local"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("TOKIO_WORKER_THREADS", "2")
            .env("NO_PROXY", "127.0.0.1,localhost");
        for key in [
            "NENIA_VAULT",
            "NENIA_REMOTE",
            "NENIA_TOKEN",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "PI_CODING_AGENT_DIR",
            "PI_CODING_AGENT_SESSION_DIR",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            command.env_remove(key);
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("start CLI")
    }

    fn ok(&self, args: &[&str]) -> Output {
        success(self.run(args))
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = success(self.command().arg("--json").args(args).output().unwrap());
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid JSON output: {error}: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn source(&self, relative: &str, bytes: &[u8]) -> PathBuf {
        let path = self.home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn connect(&self, remote: &str) {
        success(
            self.command()
                .args(["connect", remote])
                .env("NENIA_TOKEN", TOKEN)
                .output()
                .unwrap(),
        );
    }
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "CLI failed {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

struct Server {
    child: Child,
    remote: String,
}

impl Server {
    fn start(fixture: &Fixture) -> Self {
        let mut child = fixture
            .command()
            .args(["serve", "--listen", "127.0.0.1:0"])
            .env("NENIA_TOKEN", TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(rest) = line.strip_prefix("Nenia listening on ") {
                    let address = rest.split_whitespace().next().unwrap().to_owned();
                    let _ = send.send(Ok(format!("http://{address}")));
                } else {
                    let _ = send.send(Err(line));
                }
            }
        });
        match receive.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(remote)) => Self { child, remote },
            failure => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("server did not become ready: {failure:?}");
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn discover_backup_real_server_fresh_device_and_byte_exact_restore() {
    let client = Fixture::new();
    let server_fixture = Fixture::new();
    let receiver = Fixture::new();
    let records: [(&str, &[u8], &str); 3] = [
        (".codex/sessions/2026/01/codex.jsonl", b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-synthetic\",\"cwd\":\"/fiction\"}}\r\n{\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"content\":[{\"text\":\"Copper archive\"}]}}\ninvalid\xff\x00", "codex"),
        (".claude/projects/project/session/subagents/agent-test.jsonl", b"{\"type\":\"user\",\"sessionId\":\"claude-synthetic\",\"message\":{\"role\":\"user\",\"content\":\"Map the vault\"}}\n", "claude"),
        (".pi/agent/sessions/project/session.jsonl", b"{\"type\":\"session\",\"version\":3,\"id\":\"pi-synthetic\",\"cwd\":\"/fiction\"}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"Keep every byte\"}}", "pi"),
    ];
    for (relative, bytes, _) in records {
        client.source(relative, bytes);
    }
    assert_eq!(client.json(&["discover"]).as_array().unwrap().len(), 3);
    let server = Server::start(&server_fixture);
    client.connect(&server.remote);
    client.ok(&["backup"]); // Uses saved token and URL, with no source configuration.
    receiver.connect(&server.remote);
    receiver.ok(&["pull"]);
    let rows = receiver.json(&["list"]);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), records.len());
    for (index, (relative, bytes, source)) in records.iter().enumerate() {
        let row = rows.iter().find(|row| row["source"] == *source).unwrap();
        let id = row["id"].as_str().unwrap();
        let output = receiver.root.join(format!("restored-{index}"));
        receiver.ok(&["restore", id, "--output", text(&output)]);
        assert_eq!(fs::read(output).unwrap(), *bytes);
        assert_eq!(fs::read(client.home.join(relative)).unwrap(), *bytes);
        assert_eq!(receiver.ok(&["cat", id]).stdout, *bytes);
    }
    assert_eq!(receiver.json(&["verify"])["snapshots"], 3);
    receiver.ok(&["index"]);
    assert_eq!(
        receiver
            .json(&["search", "every byte"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn offline_capture_survives_source_removal_and_retries_to_saved_connection() {
    let client = Fixture::new();
    let source = client.source(
        ".codex/sessions/offline.jsonl",
        b"offline trace\r\n\xff\x00last line",
    );
    let original = fs::read(&source).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let offline_remote = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    client.connect(&offline_remote);
    let failed = client.run(&["backup"]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("offline spool"));
    let rows = client.json(&["list"]);
    let id = rows[0]["id"].as_str().unwrap();
    assert_eq!(client.ok(&["cat", id]).stdout, original);
    fs::remove_file(source).unwrap();
    let server_fixture = Fixture::new();
    let server = Server::start(&server_fixture);
    client.connect(&server.remote);
    client.ok(&["push"]);
    client.ok(&["push"]); // Idempotent when objects are already present.
    let receiver = Fixture::new();
    receiver.connect(&server.remote);
    receiver.ok(&["pull"]);
    assert_eq!(receiver.ok(&["cat", id]).stdout, original);
    assert_eq!(
        receiver
            .json(&["list", "--all-versions"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn watch_once_returns_without_sleeping_and_rescan_deduplicates() {
    let fixture = Fixture::new();
    fixture.source(".pi/agent/sessions/task.jsonl", b"one trace\n");
    let remote = url::Url::from_directory_path(fixture.root.join("remote"))
        .unwrap()
        .to_string();
    let start = Instant::now();
    for _ in 0..2 {
        fixture.ok(&["watch", "--once", "--interval", "3600", "--remote", &remote]);
    }
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(
        fixture
            .json(&["list", "--all-versions"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn wrapper_preserves_success_and_failure_exit_codes_when_upload_is_pending() {
    let fixture = Fixture::new();
    let source = fixture.source("wrapper.jsonl", b"wrapper trace\n");
    for code in [7, 0] {
        let mut command = fixture.command();
        command.args(["run", "--path", text(&source), "--"]);
        #[cfg(unix)]
        command.args(["sh", "-c", &format!("exit {code}")]);
        #[cfg(windows)]
        command.args(["cmd", "/C", "exit", "/B", &code.to_string()]);
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "wrapper changed child exit status: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("backup"));
    }
    assert_eq!(fixture.json(&["list"]).as_array().unwrap().len(), 1);
}

#[test]
fn reverting_a_source_is_a_new_latest_snapshot() {
    let fixture = Fixture::new();
    let source = fixture.source("reverted.txt", b"first state\n");
    for bytes in [
        b"first state\n".as_slice(),
        b"second state\n",
        b"first state\n",
    ] {
        fs::write(&source, bytes).unwrap();
        fixture.ok(&["ingest", text(&source)]);
    }
    let latest = fixture.json(&["list"]);
    let id = latest[0]["id"].as_str().unwrap();
    assert_eq!(fixture.ok(&["cat", id]).stdout, b"first state\n");
    assert_eq!(
        fixture
            .json(&["list", "--all-versions"])
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(fixture.json(&["verify"])["snapshots"], 3);
}

#[test]
fn explicit_vault_does_not_require_home_environment() {
    let fixture = Fixture::new();
    success(
        fixture
            .command()
            .arg("init")
            .env_remove("HOME")
            .env_remove("USERPROFILE")
            .env_remove("LOCALAPPDATA")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap(),
    );
}

#[test]
fn explicit_trace_paths_still_autodetect_their_source() {
    let fixture = Fixture::new();
    let path = fixture.source(
        "downloaded.jsonl",
        br#"{"type":"session_meta","payload":{"id":"synthetic-download"}}"#,
    );
    fixture.ok(&["ingest", text(&path)]);
    assert_eq!(fixture.json(&["list"])[0]["source"], "codex");
}

#[test]
fn catalog_loss_can_be_rebuilt_from_cli_and_corruption_never_clobbers_restore() {
    let fixture = Fixture::new();
    let path = fixture.source("trace.jsonl", b"recoverable exact bytes\xff\n");
    fixture.ok(&["ingest", text(&path)]);
    let id = fixture.json(&["list"])[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_file(fixture.vault.join("catalog.sqlite3")).unwrap();
    fixture.ok(&["rebuild"]);
    assert_eq!(fixture.ok(&["cat", &id]).stdout, fs::read(path).unwrap());
    let manifest = fixture.json(&["show", &id]);
    let pack = manifest["chunks"][0]["pack"].as_str().unwrap();
    fs::write(
        fixture.vault.join("packs").join(format!("{pack}.nz")),
        b"corrupt",
    )
    .unwrap();
    let output = fixture.root.join("restored");
    assert!(!fixture
        .run(&["restore", &id, "--output", text(&output)])
        .status
        .success());
    assert!(!output.exists());
    fs::write(&output, b"user-owned file").unwrap();
    assert!(!fixture
        .run(&["restore", &id, "--output", text(&output)])
        .status
        .success());
    assert_eq!(fs::read(&output).unwrap(), b"user-owned file");
    assert!(!fixture.run(&["verify"]).status.success());
    assert!(!fixture.run(&["rebuild"]).status.success());
    assert_eq!(fixture.json(&["list"]).as_array().unwrap().len(), 1);
}

#[test]
fn two_devices_with_the_same_origin_remain_separate_after_remote_restore() {
    let first = Fixture::new();
    let second = Fixture::new();
    let receiver = Fixture::new();
    let source = first.source(
        "shared-origin.jsonl",
        b"same path and bytes across two device identities\n",
    );
    let remote = url::Url::from_directory_path(receiver.root.join("remote"))
        .unwrap()
        .to_string();
    for client in [&first, &second] {
        client.ok(&["backup", text(&source), "--remote", &remote]);
    }
    receiver.ok(&["pull", "--remote", &remote]);
    let rows = receiver.json(&["list"]);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0]["device_id"], rows[1]["device_id"]);
    assert_eq!(rows[0]["origin"], rows[1]["origin"]);
    for row in rows {
        assert_eq!(
            receiver.ok(&["cat", row["id"].as_str().unwrap()]).stdout,
            fs::read(&source).unwrap()
        );
    }
}

#[test]
fn corrupted_catalog_is_quarantined_and_recovered_without_source_files() {
    let fixture = Fixture::new();
    let source = fixture.source("recover.jsonl", b"preserve independently of SQLite\xff\r\n");
    let bytes = fs::read(&source).unwrap();
    fixture.ok(&["ingest", text(&source)]);
    let id = fixture.json(&["list"])[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_file(source).unwrap();
    let corruption = b"deliberately invalid SQLite catalog";
    fs::write(fixture.vault.join("catalog.sqlite3"), corruption).unwrap();
    fixture.ok(&["rebuild"]);
    assert_eq!(fixture.ok(&["cat", &id]).stdout, bytes);
    let saved = fs::read_dir(&fixture.vault)
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("catalog-before-rebuild-")
        })
        .expect("old catalog should remain available for inspection");
    assert_eq!(
        fs::read(saved.path().join("catalog.sqlite3")).unwrap(),
        corruption
    );
    assert_eq!(fixture.json(&["verify"])["snapshots"], 1);
}

#[test]
fn backup_json_is_one_document_and_full_scan_detects_preserved_timestamps() {
    let fixture = Fixture::new();
    let source = fixture.source("same-size.txt", b"first");
    let remote = url::Url::from_directory_path(fixture.root.join("remote"))
        .unwrap()
        .to_string();
    let report = fixture.json(&["backup", text(&source), "--remote", &remote]);
    assert_eq!(report["capture"]["archived"], 1);
    assert!(report["upload"]["transferred_objects"].as_u64().unwrap() > 0);
    let modified = fs::metadata(&source).unwrap().modified().unwrap();
    fs::write(&source, b"other").unwrap();
    fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    fixture.ok(&["ingest", text(&source)]);
    assert_eq!(
        fixture
            .json(&["list", "--all-versions"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.ok(&["ingest", "--full-scan", text(&source)]);
    let latest = fixture.json(&["list"]);
    assert_eq!(
        fixture
            .ok(&["cat", latest[0]["id"].as_str().unwrap()])
            .stdout,
        b"other"
    );
    assert_eq!(
        fixture
            .json(&["list", "--all-versions"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
