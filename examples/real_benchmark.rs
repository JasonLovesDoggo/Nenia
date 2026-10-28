//! Benchmark actual local traces. All source details and archives stay private.
use anyhow::{ensure, Context, Result};
use chrono::Utc;
use clap::{Parser, ValueEnum};
use nenia::{
    adapters::{Candidate, Source},
    remote::{self, SyncOptions},
    server,
    vault::Vault,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Capture,
    Run,
}

#[derive(Parser)]
struct Args {
    #[arg(value_enum)]
    mode: Mode,
    /// A private directory outside the repository. Contains sensitive source copies.
    #[arg(long)]
    private_root: PathBuf,
    /// Public output contains aggregate measurements only, never source details.
    #[arg(long)]
    report: PathBuf,
    #[arg(long, default_value_t = 3)]
    trials: usize,
    #[arg(long, default_value_t = 9)]
    level: i32,
}

#[derive(Serialize, Deserialize)]
struct Frozen {
    provider: String,
    original: PathBuf,
    copy: PathBuf,
    bytes: u64,
    hash: String,
}

#[derive(Serialize, Deserialize)]
struct Corpus {
    captured_at: String,
    files: Vec<Frozen>,
    empty_files_skipped: usize,
    sources_growing_during_copy: usize,
}

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

fn file_hash(path: &Path) -> Result<String> {
    let mut hash = blake3::Hasher::new();
    io::copy(&mut File::open(path)?, &mut hash)?;
    Ok(hash.finalize().to_hex().to_string())
}

fn copy_prefix(source: &Path, destination: &Path, bytes: u64) -> Result<()> {
    let mut out = private_file(destination)?;
    let copied = io::copy(&mut File::open(source)?.take(bytes), &mut out)?;
    ensure!(
        copied == bytes,
        "a source was truncated during private capture"
    );
    out.sync_all()?;
    Ok(())
}

fn aggregate(corpus: &Corpus) -> Value {
    let mut providers = BTreeMap::new();
    for provider in ["codex", "claude", "combined"] {
        let mut sizes: Vec<_> = corpus
            .files
            .iter()
            .filter(|f| provider == "combined" || f.provider == provider)
            .map(|f| f.bytes)
            .collect();
        sizes.sort_unstable();
        if sizes.is_empty() {
            continue;
        }
        let median = if sizes.len() % 2 == 1 {
            sizes[sizes.len() / 2] as f64
        } else {
            (sizes[sizes.len() / 2 - 1] as f64 + sizes[sizes.len() / 2] as f64) / 2.0
        };
        providers.insert(provider,json!({"files":sizes.len(),"bytes":sizes.iter().sum::<u64>(),"min_bytes":sizes[0],"median_bytes":median,"p90_bytes":sizes[(sizes.len()*9).div_ceil(10)-1],"max_bytes":sizes[sizes.len()-1]}));
    }
    json!({"captured_at":corpus.captured_at,"providers":providers,"empty_files_skipped":corpus.empty_files_skipped,"sources_growing_during_copy":corpus.sources_growing_during_copy,"provenance":"all nonempty JSONL files under local Codex sessions and Claude projects stores","capture":"each source copied through its observed initial length; no invented records or histories","private_source_details_published":false})
}

fn capture(args: &Args) -> Result<()> {
    ensure!(
        !args.private_root.join("corpus.json").exists(),
        "private corpus already exists"
    );
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .context("home directory unavailable")?;
    let home = PathBuf::from(home);
    let mut corpus = Corpus {
        captured_at: Utc::now().to_rfc3339(),
        files: Vec::new(),
        empty_files_skipped: 0,
        sources_growing_during_copy: 0,
    };
    private_dir(&args.private_root.join("frozen"))?;
    for (provider, root) in [
        ("codex", home.join(".codex/sessions")),
        ("claude", home.join(".claude/projects")),
    ] {
        if !root.exists() {
            continue;
        }
        let mut sources = Vec::new();
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry?;
            if entry.file_type().is_file() && entry.path().extension().is_some_and(|e| e == "jsonl")
            {
                sources.push(entry.into_path());
            }
        }
        sources.sort();
        for original in sources {
            let bytes = fs::metadata(&original)?.len();
            if bytes == 0 {
                corpus.empty_files_skipped += 1;
                continue;
            }
            let copy = args
                .private_root
                .join("frozen")
                .join(format!("trace-{:05}.jsonl", corpus.files.len()));
            copy_prefix(&original, &copy, bytes)?;
            if fs::metadata(&original)?.len() > bytes {
                corpus.sources_growing_during_copy += 1;
            }
            let hash = file_hash(&copy)?;
            corpus.files.push(Frozen {
                provider: provider.into(),
                original,
                copy,
                bytes,
                hash,
            });
        }
    }
    ensure!(!corpus.files.is_empty(), "no actual traces found");
    let mut private = private_file(&args.private_root.join("corpus.json"))?;
    serde_json::to_writer(&mut private, &corpus)?;
    private.sync_all()?;
    fs::create_dir_all(&args.report)?;
    let public = aggregate(&corpus);
    fs::write(
        args.report.join("corpus-aggregate.json"),
        serde_json::to_vec_pretty(&public)?,
    )?;
    println!("{}", serde_json::to_string(&public)?);
    Ok(())
}

fn tree_bytes(path: &Path) -> Result<u64> {
    walkdir::WalkDir::new(path)
        .into_iter()
        .try_fold(0, |sum, entry| {
            let e = entry?;
            Ok(sum
                + if e.file_type().is_file() {
                    e.metadata()?.len()
                } else {
                    0
                })
        })
}

fn candidates(files: &[&Frozen], active: &Path) -> Result<Vec<Candidate>> {
    private_dir(active)?;
    files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let path = active.join(format!("trace-{i:05}.jsonl"));
            copy_prefix(&file.copy, &path, file.bytes)?;
            Ok(Candidate {
                path,
                source: if file.provider == "codex" {
                    Source::Codex
                } else {
                    Source::Claude
                },
            })
        })
        .collect()
}

/// Compare exported bytes without allocating an entire trace in memory.
struct Oracle {
    file: File,
    bytes: u64,
}
impl Write for Oracle {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut scratch = [0; 65536];
        for part in bytes.chunks(scratch.len()) {
            self.file.read_exact(&mut scratch[..part.len()])?;
            if &scratch[..part.len()] != part {
                return Err(io::Error::other("restored bytes differ"));
            }
        }
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn verify(vault: &Vault, ids: &[String], files: &[&Frozen]) -> Result<usize> {
    ensure!(ids.len() == files.len(), "restore count mismatch");
    for (id, file) in ids.iter().zip(files) {
        let mut oracle = Oracle {
            file: File::open(&file.copy)?,
            bytes: 0,
        };
        vault.export(id, &mut oracle)?;
        ensure!(
            oracle.bytes == file.bytes && oracle.file.read(&mut [0])? == 0,
            "restore length mismatch"
        );
    }
    Ok(ids.len())
}

fn compare_stream(reader: &mut impl Read, file: &Frozen) -> Result<()> {
    let mut oracle = Oracle {
        file: File::open(&file.copy)?,
        bytes: 0,
    };
    ensure!(
        io::copy(&mut reader.take(file.bytes), &mut oracle)? == file.bytes,
        "baseline restore truncated"
    );
    ensure!(
        oracle.file.read(&mut [0])? == 0,
        "baseline oracle length mismatch"
    );
    Ok(())
}

fn encoder(
    path: &Path,
    bytes: u64,
    level: i32,
) -> Result<zstd::stream::write::Encoder<'static, File>> {
    let mut out = zstd::stream::write::Encoder::new(private_file(path)?, level)?;
    out.window_log(23)?;
    out.include_checksum(true)?;
    out.set_pledged_src_size(Some(bytes))?;
    Ok(out)
}

fn finish(out: zstd::stream::write::Encoder<'_, File>, path: &Path) -> Result<()> {
    out.finish()?.sync_all()?;
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Serialize)]
struct Sample {
    cohort: String,
    candidate: String,
    trial: usize,
    order: usize,
    started_at: String,
    finished_at: String,
    files: usize,
    input_bytes: u64,
    elapsed_seconds: f64,
    input_mib_per_second: f64,
    payload_bytes: u64,
    manifest_bytes: u64,
    archive_bytes: u64,
    catalog_bytes: u64,
    total_bytes: u64,
    unique_chunk_bytes: Option<u64>,
    added_index_bytes: Option<u64>,
    indexed_total_bytes: Option<u64>,
    index_seconds: Option<f64>,
    unchanged_capture_seconds: Option<f64>,
    unchanged_snapshots: Option<usize>,
    unchanged_archive_growth_bytes: Option<u64>,
    metadata_poll_seconds: Option<f64>,
    metadata_poll_input_bytes: Option<u64>,
    restored_files: usize,
    errors: usize,
}

struct Retained {
    workspace: tempfile::TempDir,
    ids: Vec<String>,
    inputs: Vec<Candidate>,
}

fn measure(
    files: &[&Frozen],
    cohort: &str,
    candidate: &str,
    trial: usize,
    order: usize,
    args: &Args,
) -> Result<(Sample, Option<Retained>)> {
    let workspace = tempfile::tempdir_in(&args.private_root)?;
    let archive = workspace.path().join("archive");
    private_dir(&archive)?;
    let input_bytes = files.iter().map(|f| f.bytes).sum();
    let mut s = Sample {
        cohort: cohort.into(),
        candidate: candidate.into(),
        trial,
        order,
        started_at: Utc::now().to_rfc3339(),
        finished_at: String::new(),
        files: files.len(),
        input_bytes,
        elapsed_seconds: 0.0,
        input_mib_per_second: 0.0,
        payload_bytes: 0,
        manifest_bytes: 0,
        archive_bytes: 0,
        catalog_bytes: 0,
        total_bytes: 0,
        unique_chunk_bytes: None,
        added_index_bytes: None,
        indexed_total_bytes: None,
        index_seconds: None,
        unchanged_capture_seconds: None,
        unchanged_snapshots: None,
        unchanged_archive_growth_bytes: None,
        metadata_poll_seconds: None,
        metadata_poll_input_bytes: None,
        restored_files: 0,
        errors: 0,
    };
    let mut elapsed = Duration::ZERO;
    let retained;
    if candidate == "nenia" {
        let inputs = candidates(files, &workspace.path().join("active"))?;
        let started = Instant::now();
        let mut vault = Vault::init(&archive, args.level)?;
        let report = vault.ingest(&inputs, Source::Auto)?;
        drop(vault);
        elapsed += started.elapsed();
        ensure!(
            report.archived == files.len() && report.unchanged == 0,
            "initial capture count mismatch"
        );
        s.total_bytes = tree_bytes(&archive)?;
        s.catalog_bytes = fs::metadata(archive.join("catalog.sqlite3"))?.len();
        let mut vault = Vault::open(&archive)?;
        let stats = vault.stats()?;
        ensure!(
            stats.logical_bytes == input_bytes,
            "catalog input bytes mismatch"
        );
        s.payload_bytes = stats.pack_bytes;
        s.manifest_bytes = stats.manifest_bytes;
        s.archive_bytes = stats.archive_bytes;
        s.unique_chunk_bytes = Some(stats.unique_chunk_bytes);
        s.restored_files = verify(&vault, &report.snapshots, files)?;
        let started = Instant::now();
        let unchanged = vault.ingest(&inputs, Source::Auto)?;
        s.unchanged_capture_seconds = Some(started.elapsed().as_secs_f64());
        ensure!(
            unchanged.archived == 0 && unchanged.unchanged == files.len(),
            "unchanged capture created snapshots"
        );
        s.unchanged_snapshots = Some(unchanged.unchanged);
        let later = vault.stats()?;
        ensure!(
            later.archive_bytes == s.archive_bytes,
            "unchanged capture grew archive"
        );
        s.unchanged_archive_growth_bytes = Some(0);
        let started = Instant::now();
        let poll = vault.ingest_changed(&inputs, Source::Auto)?;
        s.metadata_poll_seconds = Some(started.elapsed().as_secs_f64());
        s.metadata_poll_input_bytes = Some(poll.input_bytes);
        ensure!(
            poll.archived == 0 && poll.unchanged == files.len() && poll.input_bytes == 0,
            "unchanged metadata poll read payloads"
        );
        let started = Instant::now();
        let indexed = vault.index(8 * 1024 * 1024)?;
        drop(vault);
        s.index_seconds = Some(started.elapsed().as_secs_f64());
        ensure!(indexed == files.len(), "index count mismatch");
        let total = tree_bytes(&archive)?;
        s.indexed_total_bytes = Some(total);
        s.added_index_bytes = Some(total.saturating_sub(s.total_bytes));
        retained = Some(Retained {
            workspace,
            ids: report.snapshots,
            inputs,
        });
    } else {
        let started = Instant::now();
        let solid = candidate == "solid_zstd";
        let mut stream = if solid {
            Some(encoder(
                &archive.join("solid.zst"),
                input_bytes,
                args.level,
            )?)
        } else {
            None
        };
        let mut mapping = Vec::new();
        let mut offset = 0u64;
        for (i, file) in files.iter().enumerate() {
            let name = if solid {
                "solid.zst".into()
            } else {
                format!("trace-{i:05}.zst")
            };
            if let Some(stream) = stream.as_mut() {
                io::copy(&mut File::open(&file.copy)?, stream)?;
            } else {
                let mut out = encoder(&archive.join(&name), file.bytes, args.level)?;
                io::copy(&mut File::open(&file.copy)?, &mut out)?;
                finish(out, &archive)?;
            }
            mapping.push(json!({"ordinal":i,"object":name,"offset":if solid {offset}else{0},"bytes":file.bytes,"hash":file_hash(&file.copy)?}));
            offset += file.bytes;
        }
        if let Some(out) = stream {
            finish(out, &archive)?;
        }
        s.payload_bytes = tree_bytes(&archive)?;
        let raw = serde_json::to_vec(&mapping)?;
        let mut out = encoder(
            &archive.join("manifest.json.zst"),
            raw.len() as u64,
            args.level,
        )?;
        out.write_all(&raw)?;
        finish(out, &archive)?;
        elapsed += started.elapsed();
        s.manifest_bytes = fs::metadata(archive.join("manifest.json.zst"))?.len();
        s.archive_bytes = s.payload_bytes + s.manifest_bytes;
        s.total_bytes = tree_bytes(&archive)?;
        let recovered: Vec<Value> = serde_json::from_slice(&zstd::stream::decode_all(
            File::open(archive.join("manifest.json.zst"))?,
        )?)?;
        ensure!(
            recovered.len() == files.len(),
            "baseline mapping count mismatch"
        );
        if solid {
            let mut decoded =
                zstd::stream::read::Decoder::new(File::open(archive.join("solid.zst"))?)?;
            for file in files {
                compare_stream(&mut decoded, file)?;
                s.restored_files += 1;
            }
            ensure!(
                decoded.read(&mut [0])? == 0,
                "solid baseline has extra bytes"
            );
        } else {
            for (file, entry) in files.iter().zip(&recovered) {
                let mut decoded = zstd::stream::read::Decoder::new(File::open(
                    archive.join(
                        entry["object"]
                            .as_str()
                            .context("baseline object missing")?,
                    ),
                )?)?;
                compare_stream(&mut decoded, file)?;
                ensure!(
                    decoded.read(&mut [0])? == 0,
                    "per-file baseline has extra bytes"
                );
                s.restored_files += 1;
            }
        }
        retained = None;
    }
    s.elapsed_seconds = elapsed.as_secs_f64();
    s.input_mib_per_second = input_bytes as f64 / 1048576.0 / s.elapsed_seconds;
    s.finished_at = Utc::now().to_rfc3339();
    Ok((s, retained))
}

async fn recovery(retained: &Retained, files: &[&Frozen], args: &Args) -> Result<Value> {
    let remote_dir = tempfile::tempdir_in(&args.private_root)?;
    let restored_dir = tempfile::tempdir_in(&args.private_root)?;
    let mut random = [0; 32];
    getrandom::fill(&mut random).map_err(|_| anyhow::anyhow!("random token generation failed"))?;
    let token = blake3::hash(&random).to_hex().to_string();
    let router = server::router(Vault::init(remote_dir.path(), args.level)?, token.clone())?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    let local = Vault::open(&retained.workspace.path().join("archive"))?;
    let options = SyncOptions {
        token: Some(token),
        ..Default::default()
    };
    let pushed = remote::push(&local.root, &endpoint, &options).await?;
    let mut fresh = Vault::init(restored_dir.path(), args.level)?;
    let pulled = remote::pull(&fresh.root, &endpoint, &options).await?;
    let rebuilt = fresh.rebuild()?;
    let first = verify(&fresh, &retained.ids, files)?;
    drop(fresh);
    fs::remove_file(restored_dir.path().join("catalog.sqlite3"))?;
    let fresh = Vault::open(restored_dir.path())?;
    let second = verify(&fresh, &retained.ids, files)?;
    task.abort();
    let _ = task.await;
    Ok(
        json!({"http_push_transferred_objects":pushed.transferred_objects,"http_push_transferred_bytes":pushed.transferred_bytes,"http_pull_transferred_objects":pulled.transferred_objects,"http_pull_transferred_bytes":pulled.transferred_bytes,"rebuilt_snapshots":rebuilt,"http_recovery_exact_restores":first,"deleted_catalog_exact_restores":second,"transport":"authenticated HTTP over loopback; correctness test, not WAN performance","errors":0}),
    )
}

fn actual_growth(retained: &Retained, files: &[&Frozen]) -> Result<Value> {
    let mut changed = Vec::new();
    let mut expected = Vec::new();
    let mut added = 0u64;
    let mut rewritten = 0usize;
    let directory = retained.workspace.path().join("actual-appends");
    private_dir(&directory)?;
    for (i, file) in files.iter().enumerate() {
        let bytes = fs::metadata(&file.original)?.len();
        if bytes <= file.bytes {
            continue;
        }
        let mut prefix = blake3::Hasher::new();
        io::copy(
            &mut File::open(&file.original)?.take(file.bytes),
            &mut prefix,
        )?;
        if prefix.finalize().to_hex().as_str() != file.hash {
            rewritten += 1;
            continue;
        }
        let copy = directory.join(format!("trace-{i:05}.jsonl"));
        copy_prefix(&file.original, &copy, bytes)?;
        fs::copy(&copy, &retained.inputs[i].path)?;
        expected.push(Frozen {
            provider: file.provider.clone(),
            original: file.original.clone(),
            hash: file_hash(&copy)?,
            copy,
            bytes,
        });
        changed.push(retained.inputs[i].clone());
        added += bytes - file.bytes;
    }
    if changed.is_empty() {
        return Ok(
            json!({"observed_actual_appends":0,"rewritten_growing_files_not_tested":rewritten,"capture_exercised":false,"reason":"no original trace acquired an observed append during this run; no history was fabricated"}),
        );
    }
    let mut vault = Vault::open(&retained.workspace.path().join("archive"))?;
    let before = vault.stats()?.archive_bytes;
    let started = Instant::now();
    let report = vault.ingest(&changed, Source::Auto)?;
    let seconds = started.elapsed().as_secs_f64();
    ensure!(
        report.archived == changed.len(),
        "actual append capture count mismatch"
    );
    let refs = expected.iter().collect::<Vec<_>>();
    let restored = verify(&vault, &report.snapshots, &refs)?;
    let initial_still_restorable = verify(&vault, &retained.ids, files)?;
    Ok(
        json!({"observed_actual_appends":changed.len(),"actual_new_source_bytes":added,"archive_growth_bytes":vault.stats()?.archive_bytes-before,"capture_seconds":seconds,"exact_new_version_restores":restored,"exact_initial_version_restores":initial_still_restorable,"rewritten_growing_files_not_tested":rewritten,"capture_exercised":true}),
    )
}

fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn run(args: &Args) -> Result<()> {
    ensure!(args.trials > 0 && args.trials <= 10, "trials must be 1..10");
    let corpus: Corpus =
        serde_json::from_reader(File::open(args.private_root.join("corpus.json"))?)?;
    for file in &corpus.files {
        ensure!(
            fs::metadata(&file.copy)?.len() == file.bytes && file_hash(&file.copy)? == file.hash,
            "private frozen corpus changed"
        );
    }
    ensure!(
        !args.report.join("samples.jsonl").exists(),
        "aggregate samples already exist; select a new report directory"
    );
    fs::create_dir_all(&args.report)?;
    fs::write(
        args.report.join("corpus-aggregate.json"),
        serde_json::to_vec_pretty(&aggregate(&corpus))?,
    )?;
    fs::write(
        args.report.join("environment.json"),
        serde_json::to_vec_pretty(
            &json!({"started_at":Utc::now().to_rfc3339(),"source_commit":output("git",&["rev-parse","HEAD"]),"rustc":output("rustc",&["-Vv"]),"zstd_version":zstd::zstd_safe::version_string(),"os":std::env::consts::OS,"arch":std::env::consts::ARCH,"logical_cpus":std::thread::available_parallelism().ok().map(|x|x.get()),"level":args.level,"window_log":23,"trials":args.trials,"private_trace_details_in_report":false,"state":"fresh archives, frozen actual files, warm source cache; no cold-storage claim","limitations":["shared development host, power/thermal state uncontrolled","file lengths, not filesystem allocated blocks or cloud billing","short batch throughput, not sustained service capacity","baseline recovery mapping published at final completion, Nenia per ingestion transaction","all source mappings, filenames, content hashes, credentials and objects remain outside the repository"]}),
        )?,
    )?;
    let mut raw = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(args.report.join("samples.jsonl"))?;
    let mut samples = Vec::new();
    let mut retained = None;
    let options = ["per_file_zstd", "solid_zstd", "nenia"];
    for trial in 1..=args.trials {
        for cohort in ["claude", "codex", "combined"] {
            let files: Vec<_> = corpus
                .files
                .iter()
                .filter(|f| cohort == "combined" || f.provider == cohort)
                .collect();
            if files.is_empty() {
                continue;
            }
            for order in 0..3 {
                let candidate = options[(order + trial - 1) % 3];
                eprintln!("trial={trial} cohort={cohort} candidate={candidate}");
                let (sample, artifact) =
                    measure(&files, cohort, candidate, trial, order + 1, args)?;
                serde_json::to_writer(&mut raw, &sample)?;
                writeln!(raw)?;
                raw.sync_data()?;
                samples.push(sample);
                if cohort == "combined" && trial == args.trials && candidate == "nenia" {
                    retained = artifact;
                }
            }
        }
    }
    let files = corpus.files.iter().collect::<Vec<_>>();
    let retained = retained.context("combined archive missing")?;
    let recovery = recovery(&retained, &files, args).await?;
    fs::write(
        args.report.join("recovery.json"),
        serde_json::to_vec_pretty(&recovery)?,
    )?;
    let growth = actual_growth(&retained, &files)?;
    fs::write(
        args.report.join("observed-growth.json"),
        serde_json::to_vec_pretty(&growth)?,
    )?;
    let mut summary = BTreeMap::new();
    for cohort in ["claude", "codex", "combined"] {
        for candidate in options {
            let rows: Vec<_> = samples
                .iter()
                .filter(|s| s.cohort == cohort && s.candidate == candidate)
                .collect();
            if rows.is_empty() {
                continue;
            }
            let mut times: Vec<_> = rows.iter().map(|s| s.elapsed_seconds).collect();
            times.sort_by(f64::total_cmp);
            let median = if times.len() % 2 == 1 {
                times[times.len() / 2]
            } else {
                (times[times.len() / 2 - 1] + times[times.len() / 2]) / 2.0
            };
            summary.insert(format!("{cohort}/{candidate}"),json!({"trials":rows.len(),"files":rows[0].files,"input_bytes":rows[0].input_bytes,"seconds_min_median_max":[times[0],median,times[times.len()-1]],"archive_bytes_min":rows.iter().map(|s|s.archive_bytes).min(),"archive_bytes_max":rows.iter().map(|s|s.archive_bytes).max(),"total_bytes_min":rows.iter().map(|s|s.total_bytes).min(),"total_bytes_max":rows.iter().map(|s|s.total_bytes).max(),"exact_restores":rows.iter().map(|s|s.restored_files).sum::<usize>(),"errors":0}));
        }
    }
    fs::write(
        args.report.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    println!(
        "{}",
        json!({"completed_batches":samples.len(),"exact_measured_restores":samples.iter().map(|s|s.restored_files).sum::<usize>(),"recovery":recovery,"observed_growth":growth,"errors":0})
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    private_dir(&args.private_root)?;
    ensure!(
        !fs::canonicalize(&args.private_root)?
            .starts_with(fs::canonicalize(std::env::current_dir()?)?),
        "private corpus must be outside the repository"
    );
    let result = match args.mode {
        Mode::Capture => capture(&args),
        Mode::Run => run(&args).await,
    };
    if let Err(error) = result {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(args.private_root.join("private-errors.log"))?;
        writeln!(log, "{} {error:#}", Utc::now().to_rfc3339())?;
        eprintln!(
            "Real benchmark failed; detailed error retained only in the private working directory."
        );
        std::process::exit(1);
    }
    Ok(())
}
