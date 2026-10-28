//! Event-driven Claude Code backups. Hook callbacks enqueue; detached workers do I/O.
use crate::{
    adapters::{Candidate, Source},
    pack,
    remote::{self, SyncOptions},
    vault::Vault,
};
use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(not(windows))]
use std::process::{Command, Stdio};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const MARKER: &str = "Nenia trace backup (managed)";
const MAX_INPUT: u64 = 1024 * 1024;
const MAX_SETTINGS: u64 = 4 * 1024 * 1024;
const EVENTS: [&str; 2] = ["Stop", "SessionEnd"];

#[derive(Debug, Serialize)]
pub struct SettingsReport {
    pub settings: PathBuf,
    pub changed: bool,
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize)]
pub struct DrainReport {
    pub captured_events: usize,
    pub uploaded_objects: u64,
    pub worker_active: bool,
}

#[derive(Deserialize)]
struct ClaudeEvent {
    session_id: String,
    transcript_path: PathBuf,
    hook_event_name: String,
    cwd: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Job {
    version: u32,
    transcript: PathBuf,
    session_id: String,
    event: String,
}

pub fn claude_settings(home: &Path) -> PathBuf {
    let directory = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    expand_home(&directory, home).join("settings.json")
}

fn expand_home(path: &Path, home: &Path) -> PathBuf {
    path.strip_prefix("~")
        .map(|rest| home.join(rest))
        .unwrap_or_else(|_| path.into())
}

fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "file exceeds limit: {}",
        path.display()
    );
    Ok(bytes)
}

fn lock_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn lock_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error
            .raw_os_error()
            .is_some_and(|code| Some(code) == fs2::lock_contended_error().raw_os_error())
}

fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn owned(handler: &Value) -> bool {
    handler["type"] == "command"
        && handler["statusMessage"] == MARKER
        && handler["args"].as_array().is_some_and(|args| {
            args.len() >= 2 && args[args.len() - 2] == "hook" && args[args.len() - 1] == "claude"
        })
}

/// Uses Claude's documented exec form: arguments are never interpreted by a shell.
pub fn install_claude(settings: &Path, executable: &Path, vault: &Path) -> Result<SettingsReport> {
    let executable = fs::canonicalize(executable).context("resolve Nenia executable")?;
    ensure!(executable.is_file(), "Nenia executable is not a file");
    let vault = fs::canonicalize(vault).context("initialize the vault before installing hooks")?;
    let handler = json!({
        "type":"command", "command":executable,
        "args":["--vault",vault,"hook","claude"],
        "timeout":5, "onFailure":"continue", "statusMessage":MARKER
    });
    edit_settings(settings, Some(handler))
}

pub fn uninstall_claude(settings: &Path) -> Result<SettingsReport> {
    reject_dangling_settings_link(settings)?;
    if !settings.exists() {
        return Ok(SettingsReport {
            settings: settings.into(),
            changed: false,
            backup: None,
        });
    }
    edit_settings(settings, None)
}

fn edit_settings(settings: &Path, handler: Option<Value>) -> Result<SettingsReport> {
    reject_dangling_settings_link(settings)?;
    let parent = settings
        .parent()
        .context("settings need a parent directory")?;
    fs::create_dir_all(parent)?;
    // Preserve a dotfiles symlink by replacing its target, not the link itself.
    let settings = if settings.exists() {
        fs::canonicalize(settings)?
    } else {
        fs::canonicalize(parent)?.join(settings.file_name().context("missing settings filename")?)
    };
    let parent = settings.parent().unwrap();
    let lock = lock_file(&parent.join(".nenia-hooks.lock"))?;
    lock.try_lock_exclusive()
        .context("another Nenia installer is updating these settings")?;
    let original = if settings.exists() {
        Some(bounded_read(&settings, MAX_SETTINGS)?)
    } else {
        None
    };
    let mut document: Value = match &original {
        Some(bytes) => {
            serde_json::from_slice(bytes).context("settings are not valid JSON; left unchanged")?
        }
        None => json!({}),
    };
    let before = document.clone();
    let object = document
        .as_object_mut()
        .context("settings must be a JSON object; left unchanged")?;
    if handler.is_none() && !object.contains_key("hooks") {
        return Ok(SettingsReport {
            settings,
            changed: false,
            backup: None,
        });
    }
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("existing hooks must be an object; left unchanged")?;
    let mut removed = false;
    for event in EVENTS {
        let mut event_removed = false;
        if let Some(groups) = hooks.get_mut(event) {
            let groups = groups
                .as_array_mut()
                .context("existing event hooks must be arrays; left unchanged")?;
            let mut retained = Vec::new();
            for mut group in groups.drain(..) {
                let handlers = group
                    .get_mut("hooks")
                    .and_then(Value::as_array_mut)
                    .context("existing hook groups need a hooks array; left unchanged")?;
                let old_len = handlers.len();
                handlers.retain(|entry| !owned(entry));
                event_removed |= old_len != handlers.len();
                if !handlers.is_empty() || old_len == 0 {
                    retained.push(group);
                }
            }
            *groups = retained;
        }
        removed |= event_removed;
        if let Some(handler) = &handler {
            hooks
                .entry(event)
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap()
                .push(json!({"hooks":[handler]}));
        } else if event_removed
            && hooks
                .get(event)
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
        {
            hooks.remove(event);
        }
    }
    if removed && hooks.is_empty() {
        object.remove("hooks");
    }
    if document == before {
        return Ok(SettingsReport {
            settings,
            changed: false,
            backup: None,
        });
    }
    // Detect edits from tools which do not honor our advisory lock.
    let current = if settings.exists() {
        Some(bounded_read(&settings, MAX_SETTINGS)?)
    } else {
        None
    };
    ensure!(
        current == original,
        "settings changed during installation; retry without losing the other edit"
    );
    let backup = if let Some(bytes) = &original {
        let name = format!(
            "{}.nenia-backup-{}.json",
            settings.file_name().unwrap().to_string_lossy(),
            &blake3::hash(bytes).to_hex()[..16]
        );
        let path = parent.join(name);
        pack::atomic_write_immutable(&path, bytes)?;
        Some(path)
    } else {
        None
    };
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, &document)?;
    temp.write_all(b"\n")?;
    if let Ok(metadata) = fs::metadata(&settings) {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.as_file().sync_all()?;
    let latest = if settings.exists() {
        Some(bounded_read(&settings, MAX_SETTINGS)?)
    } else {
        None
    };
    ensure!(
        latest == original,
        "settings changed during installation; left the other edit intact"
    );
    temp.persist(&settings)?;
    sync_dir(parent)?;
    Ok(SettingsReport {
        settings,
        changed: true,
        backup,
    })
}

fn reject_dangling_settings_link(settings: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(settings) {
        ensure!(
            !metadata.file_type().is_symlink() || settings.exists(),
            "settings is a dangling symlink; left unchanged"
        );
    }
    Ok(())
}

fn queue_dir(root: &Path) -> PathBuf {
    root.join("hooks").join("pending")
}

fn ensure_queue(root: &Path) -> Result<()> {
    ensure!(
        root.join("config.json").is_file(),
        "initialize/connect the Nenia vault before using hooks"
    );
    fs::create_dir_all(queue_dir(root))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [root.join("hooks"), queue_dir(root)] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    sync_dir(root)?;
    sync_dir(&root.join("hooks"))?;
    Ok(())
}

fn queue_lock(root: &Path) -> Result<File> {
    let file = lock_file(&root.join("hooks").join("queue.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn worker_lock(root: &Path) -> Result<File> {
    lock_file(&root.join("hooks").join("worker.lock"))
}

/// Persist only the transcript path from official Stop/SessionEnd stdin JSON.
pub fn queue_claude(root: &Path, home: &Path, input: impl Read) -> Result<PathBuf> {
    let mut bytes = Vec::new();
    input.take(MAX_INPUT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_INPUT,
        "Claude hook input exceeds 1 MiB"
    );
    let event: ClaudeEvent = serde_json::from_slice(&bytes).context("invalid Claude hook JSON")?;
    ensure!(
        EVENTS.contains(&event.hook_event_name.as_str()),
        "only Stop and SessionEnd events are accepted"
    );
    ensure!(
        !event.session_id.is_empty() && event.session_id.len() <= 256,
        "invalid session_id"
    );
    let mut path = expand_home(&event.transcript_path, home);
    if path.is_relative() {
        path = event
            .cwd
            .filter(|cwd| cwd.is_absolute())
            .context("relative transcript_path needs absolute cwd")?
            .join(path);
    }
    ensure!(
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl")),
        "Claude transcript must be a JSONL file"
    );
    ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "Claude transcript is not a regular file"
    );
    let path = fs::canonicalize(path)?;
    ensure!(
        !path.starts_with(fs::canonicalize(root)?),
        "refusing to capture a transcript inside the vault"
    );
    ensure_queue(root)?;
    let _gate = queue_lock(root)?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("generate hook request ID: {e}"))?;
    let name = blake3::hash(&random).to_hex().to_string();
    let destination = queue_dir(root).join(format!("{name}.json"));
    let job = Job {
        version: 1,
        transcript: path,
        session_id: event.session_id,
        event: event.hook_event_name,
    };
    pack::atomic_write_immutable(&destination, &serde_json::to_vec(&job)?)?;
    Ok(destination)
}

/// Coalesce wakeups while a worker is active; the queue gate closes the exit race.
pub fn wake_worker(root: &Path, executable: &Path) -> Result<bool> {
    ensure_queue(root)?;
    let _gate = queue_lock(root)?;
    let worker = worker_lock(root)?;
    match worker.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if lock_busy(&error) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    #[cfg(not(windows))]
    let mut command = Command::new(executable);
    #[cfg(not(windows))]
    command
        .arg("--vault")
        .arg(fs::canonicalize(root)?)
        .args(["hooks", "drain"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe; no allocation or locks occur after fork.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    // Release the reservation before exec: a child can start before spawn returns.
    // The queue gate still prevents a producer/worker-exit race during this handoff.
    drop(worker);
    #[cfg(windows)]
    crate::windows_worker::spawn(executable, &fs::canonicalize(root)?)?;
    #[cfg(not(windows))]
    command
        .spawn()
        .context("start detached backup worker; queued event remains safe")?;
    Ok(true)
}

fn pending(root: &Path, excluded: &HashSet<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut jobs = Vec::new();
    for entry in fs::read_dir(queue_dir(root))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "json") && !excluded.contains(&path) {
            jobs.push(path);
            if jobs.len() == 256 {
                break;
            }
        }
    }
    jobs.sort();
    Ok(jobs)
}

async fn open_when_available(root: &Path, deadline: tokio::time::Instant) -> Result<Vault> {
    loop {
        match Vault::open(root) {
            Ok(vault) => return Ok(vault),
            Err(error) => {
                let busy = error
                    .chain()
                    .any(|e| e.to_string().contains("vault is busy"));
                if !busy || tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Drain queued events and retry pending uploads. No source-directory polling occurs.
pub async fn drain(root: &Path, busy_timeout: Duration) -> Result<DrainReport> {
    ensure_queue(root)?;
    let worker = worker_lock(root)?;
    match worker.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if lock_busy(&error) => {
            return Ok(DrainReport {
                worker_active: true,
                ..Default::default()
            })
        }
        Err(error) => return Err(error.into()),
    }
    let result = drain_locked(root, worker, busy_timeout).await;
    let status = match &result {
        Ok(report) => json!({"at":chrono::Utc::now().to_rfc3339(),"report":report,"error":null}),
        Err(error) => json!({"at":chrono::Utc::now().to_rfc3339(),"error":format!("{error:#}")}),
    };
    let mut temp = tempfile::NamedTempFile::new_in(root.join("hooks"))?;
    serde_json::to_writer(&mut temp, &status)?;
    temp.as_file().sync_all()?;
    temp.persist(root.join("hooks").join("last-run.json"))?;
    result
}

async fn drain_locked(root: &Path, worker: File, busy_timeout: Duration) -> Result<DrainReport> {
    let mut worker = Some(worker);
    let deadline = tokio::time::Instant::now() + busy_timeout;
    let mut report = DrainReport::default();
    let mut failed = HashSet::new();
    let mut failure = None;
    let mut uploaded_once = false;
    loop {
        let gate = queue_lock(root)?;
        let jobs = pending(root, &failed)?;
        if jobs.is_empty() && uploaded_once {
            // A producer cannot enqueue between this check and releasing the worker lock.
            drop(worker.take());
            drop(gate);
            if let Some(error) = failure {
                bail!(
                    "{error}; pending events/archives are retained; retry with `nenia hooks drain`"
                );
            }
            return Ok(report);
        }
        drop(gate);
        let mut grouped: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for path in &jobs {
            let parsed = (|| -> Result<Job> {
                let job: Job = serde_json::from_slice(&bounded_read(path, MAX_INPUT)?)?;
                ensure!(job.version == 1, "unsupported queued event version");
                Ok(job)
            })();
            match parsed {
                Ok(job) => grouped
                    .entry(job.transcript)
                    .or_default()
                    .push(path.clone()),
                Err(error) => {
                    failed.insert(path.clone());
                    failure = Some(format!("invalid queued event: {error:#}"));
                }
            }
        }
        let mut vault = match open_when_available(root, deadline).await {
            Ok(vault) => vault,
            Err(error) => {
                failed.extend(jobs);
                failure = Some(format!("could not open vault: {error:#}"));
                uploaded_once = true;
                continue;
            }
        };
        for (path, requests) in grouped {
            match vault.ingest(
                &[Candidate {
                    path,
                    source: Source::Claude,
                }],
                Source::Claude,
            ) {
                Ok(_) => {
                    let _gate = queue_lock(root)?;
                    for request in requests {
                        fs::remove_file(request)?;
                        report.captured_events += 1;
                    }
                    sync_dir(&queue_dir(root))?;
                }
                Err(error) => {
                    failed.extend(requests);
                    failure = Some(format!("capture failed: {error:#}"));
                }
            }
        }
        // Capture the entire available queue before spending time on the network.
        let gate = queue_lock(root)?;
        let more = !pending(root, &failed)?.is_empty();
        drop(gate);
        if more {
            continue;
        }
        let upload = async {
            vault.check_catalog_objects()?;
            let destination = vault
                .config
                .remote
                .clone()
                .context("no remote configured; captured bytes remain in the offline spool")?;
            let options = SyncOptions {
                concurrency: 4,
                verify_existing: false,
                token: vault.config.token.clone(),
            };
            let result = tokio::time::timeout(
                Duration::from_secs(300),
                remote::push(root, &destination, &options),
            )
            .await
            .context("upload time budget exceeded; offline spool retained")??;
            Ok::<_, anyhow::Error>(result)
        }
        .await;
        match upload {
            Ok(result) => {
                report.uploaded_objects += result.transferred_objects;
                if failed.is_empty() {
                    failure = None;
                }
            }
            Err(error) => failure = Some(format!("upload pending: {error:#}")),
        }
        uploaded_once = true;
    }
}
