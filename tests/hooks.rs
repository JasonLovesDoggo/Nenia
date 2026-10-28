use nenia::{
    hooks,
    remote::{self, SyncOptions},
    vault::Vault,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// Fork/exec can briefly inherit another test's advisory locks. Serialize these
// fixtures while exercising concurrent hook processes inside their test.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    vault: PathBuf,
    remote: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            tempfile::tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let root = directory.path().to_path_buf();
        let home = root.join("home");
        let vault = root.join("vault");
        let remote = root.join("remote");
        fs::create_dir(&home).unwrap();
        let mut opened = Vault::init(&vault, 1).unwrap();
        opened
            .connect(
                url::Url::from_directory_path(&remote).unwrap().to_string(),
                None,
            )
            .unwrap();
        drop(opened);
        Self {
            _directory: directory,
            root,
            home,
            vault,
            remote,
        }
    }

    fn trace(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.home.join(".claude/projects/synthetic").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn event(&self, transcript: &Path, event: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "session_id":"synthetic-session", "transcript_path":transcript,
            "cwd":self.home, "permission_mode":"default", "hook_event_name":event,
            "stop_hook_active":false, "last_assistant_message":"do-not-copy-the-hook-body",
            "reason":"other"
        }))
        .unwrap()
    }

    fn queue(&self, path: &Path, event: &str) -> PathBuf {
        hooks::queue_claude(
            &self.vault,
            &self.home,
            Cursor::new(self.event(path, event)),
        )
        .unwrap()
    }

    fn queued(&self) -> usize {
        fs::read_dir(self.vault.join("hooks/pending"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                    .count()
            })
            .unwrap_or(0)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nenia"));
        command
            .arg("--vault")
            .arg(&self.vault)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("TOKIO_WORKER_THREADS", "2")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("NENIA_VAULT")
            .env_remove("NENIA_REMOTE")
            .env_remove("NENIA_TOKEN");
        command
    }

    fn callback(&self, input: &[u8]) -> std::process::Output {
        let mut child = self
            .command()
            .args(["hook", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = send.send(child.wait_with_output());
        });
        receive
            .recv_timeout(Duration::from_secs(5))
            .expect(
                "hook callback pipes stayed open; a detached descendant may have inherited them",
            )
            .unwrap()
    }

    fn wait_for_snapshots(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(vault) = Vault::open(&self.vault) {
                if vault.list(None, 100, true).unwrap().len() == count && self.queued() == 0 {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "detached hook worker did not finish"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[test]
fn installer_preserves_other_settings_is_idempotent_and_uninstalls_only_its_hooks() {
    let _serial = TEST_LOCK.blocking_lock();
    let f = Fixture::new();
    let settings = f.home.join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let original = json!({"theme":"dark", "env":{"SYNTHETIC_SECRET":"private-fixture-value"},
        "hooks":{"Stop":[{"matcher":"", "hooks":[{"type":"command","command":"keep-existing-hook"}]}],
            "SessionStart":[{"hooks":[{"type":"command","command":"keep-start-hook"}]}]},
        "extraKnownMarketplaces":{"fixture":{"source":"fixture"}}});
    let original_bytes = serde_json::to_vec_pretty(&original).unwrap();
    fs::write(&settings, &original_bytes).unwrap();
    let executable = f.root.join("path with spaces ' $HOME `literal`.exe");
    fs::write(&executable, b"synthetic executable placeholder").unwrap();
    let installed = hooks::install_claude(&settings, &executable, &f.vault).unwrap();
    assert!(installed.changed);
    assert_eq!(fs::read(installed.backup.unwrap()).unwrap(), original_bytes);
    let first = fs::read(&settings).unwrap();
    let actual: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(actual["env"], original["env"]);
    assert_eq!(
        actual["hooks"]["SessionStart"],
        original["hooks"]["SessionStart"]
    );
    assert_eq!(actual["hooks"]["Stop"][0], original["hooks"]["Stop"][0]);
    let handler = &actual["hooks"]["SessionEnd"][0]["hooks"][0];
    assert_eq!(
        handler["command"],
        json!(fs::canonicalize(&executable).unwrap())
    );
    assert_eq!(
        handler["args"],
        json!([
            "--vault",
            fs::canonicalize(&f.vault).unwrap(),
            "hook",
            "claude"
        ])
    );
    assert_eq!(handler["onFailure"], "continue");
    assert!(handler.get("async").is_none()); // Only the durable enqueue runs synchronously.
    assert!(
        !hooks::install_claude(&settings, &executable, &f.vault)
            .unwrap()
            .changed
    );
    assert_eq!(fs::read(&settings).unwrap(), first);
    assert!(hooks::uninstall_claude(&settings).unwrap().changed);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&settings).unwrap()).unwrap(),
        original
    );
    let removed = fs::read(&settings).unwrap();
    assert!(!hooks::uninstall_claude(&settings).unwrap().changed);
    assert_eq!(fs::read(&settings).unwrap(), removed);
}

#[test]
fn invalid_settings_and_unrelated_empty_hook_settings_are_untouched() {
    let _serial = TEST_LOCK.blocking_lock();
    let f = Fixture::new();
    let settings = f.root.join("settings.json");
    for original in [
        b"{broken".as_slice(),
        br#"{"hooks":[]}"#,
        br#"{"hooks":{"Stop":"wrong type"}}"#,
    ] {
        fs::write(&settings, original).unwrap();
        assert!(
            hooks::install_claude(&settings, Path::new(env!("CARGO_BIN_EXE_nenia")), &f.vault)
                .is_err()
        );
        assert_eq!(fs::read(&settings).unwrap(), original);
    }
    let untouched = br#"{ "hooks": { "Stop": [] }, "theme": "light" }"#;
    fs::write(&settings, untouched).unwrap();
    assert!(!hooks::uninstall_claude(&settings).unwrap().changed);
    assert_eq!(fs::read(&settings).unwrap(), untouched);
    assert!(
        !hooks::uninstall_claude(&f.root.join("absent/settings.json"))
            .unwrap()
            .changed
    );
}

#[cfg(unix)]
#[test]
fn installer_preserves_a_settings_symlink() {
    let _serial = TEST_LOCK.blocking_lock();
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let target = f.root.join("dotfiles-settings.json");
    let link = f.root.join("settings.json");
    fs::write(&target, b"{\"theme\":\"dark\"}").unwrap();
    symlink(&target, &link).unwrap();
    hooks::install_claude(&link, Path::new(env!("CARGO_BIN_EXE_nenia")), &f.vault).unwrap();
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(
        serde_json::from_slice::<Value>(&fs::read(&target).unwrap()).unwrap()["hooks"].is_object()
    );
    fs::remove_file(&target).unwrap();
    assert!(
        hooks::install_claude(&link, Path::new(env!("CARGO_BIN_EXE_nenia")), &f.vault).is_err()
    );
    assert!(hooks::uninstall_claude(&link).is_err());
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[tokio::test]
async fn official_events_capture_only_the_requested_transcript_and_restore_remotely() {
    let _serial = TEST_LOCK.lock().await;
    let f = Fixture::new();
    let source = f.trace("session.jsonl", b"first event\r\n");
    f.trace("unrelated.jsonl", b"must not be ingested\n");
    let queued = f.queue(&source, "Stop");
    assert!(!fs::read_to_string(queued)
        .unwrap()
        .contains("do-not-copy-the-hook-body"));
    let final_bytes = b"first event\r\nlast event\xff\x00";
    fs::write(&source, final_bytes).unwrap();
    f.queue(&source, "SessionEnd");
    let result = hooks::drain(&f.vault, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(result.captured_events, 2);
    assert!(result.uploaded_objects >= 2);
    assert_eq!(f.queued(), 0);
    let mut receiver = Vault::init(&f.root.join("receiver"), 1).unwrap();
    remote::pull(
        &receiver.root,
        url::Url::from_directory_path(&f.remote).unwrap().as_str(),
        &SyncOptions::default(),
    )
    .await
    .unwrap();
    receiver.reconcile().unwrap();
    let snapshots = receiver.list(None, 100, true).unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].source, "claude");
    let mut restored = Vec::new();
    receiver.export(&snapshots[0].id, &mut restored).unwrap();
    assert_eq!(restored, final_bytes);
}

#[tokio::test]
async fn busy_vault_and_failed_capture_keep_the_final_event_for_retry() {
    let _serial = TEST_LOCK.lock().await;
    let f = Fixture::new();
    let source = f.trace("session.jsonl", b"queued while vault busy\n");
    let held = Vault::open(&f.vault).unwrap();
    f.queue(&source, "SessionEnd");
    assert!(hooks::drain(&f.vault, Duration::ZERO).await.is_err());
    assert_eq!(f.queued(), 1);
    drop(held);
    fs::remove_file(&source).unwrap();
    assert!(hooks::drain(&f.vault, Duration::ZERO).await.is_err());
    assert_eq!(f.queued(), 1);
    fs::write(&source, b"trace is accessible again\n").unwrap();
    assert_eq!(
        hooks::drain(&f.vault, Duration::ZERO)
            .await
            .unwrap()
            .captured_events,
        1
    );
    assert_eq!(f.queued(), 0);
}

#[tokio::test]
async fn offline_capture_survives_then_manual_drain_uploads_without_a_new_event() {
    let _serial = TEST_LOCK.lock().await;
    let f = Fixture::new();
    let source = f.trace("offline.jsonl", b"offline bytes retained\n");
    let mut vault = Vault::open(&f.vault).unwrap();
    vault.config.remote = None;
    // Persist a syntactically valid unsupported destination so upload fails immediately.
    vault.connect("unsupported://offline".into(), None).unwrap();
    drop(vault);
    f.queue(&source, "SessionEnd");
    assert!(hooks::drain(&f.vault, Duration::ZERO).await.is_err());
    assert_eq!(f.queued(), 0); // Capture is durable; only upload remains pending.
    fs::remove_file(source).unwrap();
    let mut vault = Vault::open(&f.vault).unwrap();
    assert_eq!(vault.list(None, 100, true).unwrap().len(), 1);
    vault
        .connect(
            url::Url::from_directory_path(&f.remote)
                .unwrap()
                .to_string(),
            None,
        )
        .unwrap();
    drop(vault);
    assert!(
        hooks::drain(&f.vault, Duration::ZERO)
            .await
            .unwrap()
            .uploaded_objects
            >= 2
    );
    assert_eq!(fs::read_dir(f.remote.join("manifests")).unwrap().count(), 1);
}

#[test]
fn detached_callbacks_return_while_busy_and_capture_both_concurrent_sessions() {
    let _serial = TEST_LOCK.blocking_lock();
    let f = Fixture::new();
    let first = f.trace("one.jsonl", b"first queued trace\n");
    let second = f.trace("two.jsonl", b"last queued trace\n");
    let held = Vault::open(&f.vault).unwrap();
    let started = Instant::now();
    for (path, event) in [(&first, "Stop"), (&second, "SessionEnd")] {
        let output = f.callback(&f.event(path, event));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "hook callback waited for the vault/network"
    );
    assert_eq!(f.queued(), 2);
    drop(held);
    f.wait_for_snapshots(2);
    assert_eq!(fs::read_dir(f.remote.join("manifests")).unwrap().count(), 2);
}

#[test]
fn cli_installer_honors_config_override_and_exec_form_accepts_literal_path_characters() {
    let _serial = TEST_LOCK.blocking_lock();
    let f = Fixture::new();
    let config = f.home.join("custom Claude's $literal `folder`");
    let output = f
        .command()
        .args(["hooks", "install", "claude"])
        .env("CLAUDE_CONFIG_DIR", &config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!f.home.join(".claude/settings.json").exists());
    let settings: Value =
        serde_json::from_slice(&fs::read(config.join("settings.json")).unwrap()).unwrap();
    let handler = &settings["hooks"]["Stop"][0]["hooks"][0];
    let source = f.trace(
        "session's $literal `name`.jsonl",
        b"literal path survives exec\n",
    );
    // Mirror Claude's documented direct-spawn command+args behavior exactly.
    let mut command = Command::new(handler["command"].as_str().unwrap());
    command
        .args(
            handler["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap()),
        )
        .env("HOME", &f.home)
        .env("USERPROFILE", &f.home)
        .env("TOKIO_WORKER_THREADS", "2")
        .env_remove("NENIA_REMOTE")
        .env_remove("NENIA_TOKEN")
        .env_remove("NENIA_VAULT");
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&f.event(&source, "Stop"))
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
    f.wait_for_snapshots(1);
}

#[test]
fn invalid_hook_payloads_are_nonblocking_and_do_not_scan_other_files() {
    let _serial = TEST_LOCK.blocking_lock();
    let f = Fixture::new();
    f.trace("unrelated.jsonl", b"never requested\n");
    for input in [
        b"not JSON".as_slice(),
        br#"{"hook_event_name":"PostToolUse","session_id":"s","transcript_path":"/missing.jsonl"}"#,
    ] {
        let output = f.callback(input);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    assert_eq!(f.queued(), 0);
    assert!(Vault::open(&f.vault)
        .unwrap()
        .list(None, 10, true)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn failed_worker_spawn_leaves_a_durable_request_for_manual_recovery() {
    let _serial = TEST_LOCK.lock().await;
    let f = Fixture::new();
    let source = f.trace("recover.jsonl", b"retry after missing executable\n");
    f.queue(&source, "SessionEnd");
    assert!(hooks::wake_worker(&f.vault, &f.root.join("missing-executable")).is_err());
    assert_eq!(f.queued(), 1);
    assert_eq!(
        hooks::drain(&f.vault, Duration::ZERO)
            .await
            .unwrap()
            .captured_events,
        1
    );
}
