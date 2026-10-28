//! Reproducible local archival benchmark. See docs/benchmarks.md for scope.
use anyhow::{ensure, Context, Result};
use chrono::Utc;
use clap::Parser;
use nenia::adapters::{Candidate, Source};
use nenia::pack::atomic_write_immutable;
use nenia::vault::Vault;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const SEED: u64 = 0x4e45_4e49_4132_3032;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 3)]
    trials: usize,
    /// Scale fixture sizes only; use 0.0625 for a correctness smoke test.
    #[arg(long, default_value_t = 1.0)]
    scale: f64,
    #[arg(long, default_value_t = 9)]
    level: i32,
    #[arg(long)]
    case: Option<String>,
}

#[derive(Clone)]
struct Trace {
    name: String,
    bytes: Vec<u8>,
}

struct Case {
    name: &'static str,
    rounds: Vec<Vec<Trace>>,
}

impl Case {
    fn bytes(&self) -> u64 {
        self.rounds
            .iter()
            .flatten()
            .map(|t| t.bytes.len() as u64)
            .sum()
    }

    fn count(&self) -> usize {
        self.rounds.iter().map(Vec::len).sum()
    }
}

#[derive(Serialize)]
struct Sample {
    case: String,
    trial: usize,
    order: usize,
    candidate: String,
    started_at: String,
    finished_at: String,
    input_bytes: u64,
    snapshots: usize,
    elapsed_seconds: f64,
    input_mib_per_second: f64,
    payload_bytes: u64,
    manifest_bytes: u64,
    archive_bytes: u64,
    catalog_bytes: u64,
    total_bytes: u64,
    indexed_total_bytes: Option<u64>,
    added_index_bytes: Option<u64>,
    index_seconds: Option<f64>,
    unique_chunk_bytes: Option<u64>,
    restored_snapshots: usize,
    errors: usize,
}

struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn hex(&mut self) -> String {
        format!("{:016x}{:016x}", self.next(), self.next())
    }
}

fn segment(seed: u64, size: usize, header: bool) -> Vec<u8> {
    let mut random = Random(seed);
    let mut result = Vec::with_capacity(size + 4096);
    if header {
        writeln!(
            result,
            "{}",
            json!({"type":"session_meta", "payload":{
                "id":random.hex(), "timestamp":"2026-01-01T00:00:00Z", "cwd":"/work/project"
            }})
        )
        .unwrap();
    }
    let services = [
        "scheduler",
        "catalog",
        "authentication",
        "payments",
        "worker",
        "cache",
        "search",
        "upload",
    ];
    let verbs = [
        "validate",
        "retry",
        "serialize",
        "lookup",
        "enqueue",
        "reconcile",
        "cancel",
        "commit",
    ];
    let mut event = 0;
    while result.len() < size {
        let service = services[random.next() as usize % services.len()];
        let verb = verbs[random.next() as usize % verbs.len()];
        let identifier = random.hex();
        let body = match event % 5 {
            0 => format!("Please fix {service} so it can {verb} request {identifier}. Preserve ordering when {} workers overlap, handle a {} millisecond timeout, and add a regression test for an interrupted transaction. The reported production request had token {}.", random.next() % 32 + 1, random.next() % 9000 + 1, random.hex()),
            1 => format!("I inspected src/{service}/state.rs and tests/{service}_{}.rs. The state transition drops a pending result before {verb} completes. I will keep the current transaction alive, validate request {identifier}, and test both the success and cancellation paths. The relevant previous deployment was {}.", random.next() % 80, random.hex()),
            2 => {
                let mut code = format!("diff --git a/src/{service}.rs b/src/{service}.rs\n@@ -{},6 +{},14 @@\n", random.next() % 900, random.next() % 900);
                for _ in 0..8 {
                    code.push_str(&format!("+ let value_{} = state.{verb}(\"{}\", {}).await?;\n", random.next() % 1024, random.hex(), random.next() % 100_000));
                }
                code
            }
            3 => {
                let mut log = String::new();
                for _ in 0..12 {
                    log.push_str(&format!("2026-01-{:02}T{:02}:{:02}:{:02}Z INFO {service} request={} elapsed_us={} shard={} result={}\n", random.next() % 28 + 1, random.next() % 24, random.next() % 60, random.next() % 60, random.hex(), random.next() % 200_000, random.next() % 256, ["accepted", "retry", "completed", "cancelled"][random.next() as usize % 4]));
                }
                log
            }
            _ => format!("test {service}::tests::{verb}_{} ... ok\ntest {service}::tests::retry_{} ... ok\nVerified {} requests across {} partitions. Input digest {}. Output digest {}. The patch preserves the existing API and makes the interrupted request observable through the normal status endpoint.", random.next() % 1000, random.next() % 1000, random.next() % 50_000, random.next() % 64 + 1, random.hex(), random.hex()),
        };
        writeln!(result, "{}", json!({
            "timestamp":format!("2026-01-01T{:02}:{:02}:{:02}.{:03}Z", event / 3600 % 24, event / 60 % 60, event % 60, random.next() % 1000),
            "type":"response_item", "event_id":identifier,
            "payload":{"type":"message", "role":if event % 5 == 0 {"user"} else {"assistant"},
                "content":[{"type":"output_text", "text":body}]}
        })).unwrap();
        event += 1;
    }
    result
}

fn cases(scale: f64) -> Vec<Case> {
    let size = |kib: usize| ((kib * 1024) as f64 * scale).max(4096.0) as usize;
    let independent = (0..16)
        .map(|i| Trace {
            name: format!("session-{i}.jsonl"),
            bytes: segment(SEED + i, size(512), true),
        })
        .collect();
    let mut current: Vec<_> = (0..6)
        .map(|i| Trace {
            name: format!("session-{i}.jsonl"),
            bytes: segment(SEED + 100 + i, size(512), true),
        })
        .collect();
    let mut append = vec![current.clone()];
    for round in 1..6 {
        for (i, trace) in current.iter_mut().enumerate() {
            trace.bytes.extend(segment(
                SEED + 200 + round * 10 + i as u64,
                size(128),
                false,
            ));
        }
        append.push(current.clone());
    }
    let prefix = segment(SEED + 500, size(1024), true);
    let forks = (0..12)
        .map(|i| {
            let mut bytes = prefix.clone();
            bytes.extend(segment(SEED + 600 + i, size(256), false));
            Trace {
                name: format!("fork-{i}.jsonl"),
                bytes,
            }
        })
        .collect();
    let entropy = (0..4)
        .map(|i| {
            let mut random = Random(SEED + 900 + i);
            let mut bytes = Vec::with_capacity(size(2048));
            while bytes.len() < size(2048) {
                bytes.extend_from_slice(&random.next().to_le_bytes());
            }
            bytes.truncate(size(2048));
            Trace {
                name: format!("entropy-{i}.bin"),
                bytes,
            }
        })
        .collect();
    vec![
        Case {
            name: "independent",
            rounds: vec![independent],
        },
        Case {
            name: "append_history",
            rounds: append,
        },
        Case {
            name: "shared_prefix_forks",
            rounds: vec![forks],
        },
        Case {
            name: "entropy",
            rounds: vec![entropy],
        },
    ]
}

fn prepare(root: &Path, round: &[Trace]) -> Result<Vec<Candidate>> {
    fs::create_dir_all(root)?;
    round
        .iter()
        .map(|trace| {
            let path = root.join(&trace.name);
            fs::write(&path, &trace.bytes)?;
            Ok(Candidate {
                path,
                source: Source::Auto,
            })
        })
        .collect()
}

fn tree_bytes(path: &Path) -> Result<u64> {
    walkdir::WalkDir::new(path)
        .into_iter()
        .try_fold(0, |total, entry| {
            let entry = entry?;
            Ok(total
                + if entry.file_type().is_file() {
                    entry.metadata()?.len()
                } else {
                    0
                })
        })
}

fn encoder(size: u64, level: i32) -> Result<zstd::stream::write::Encoder<'static, Vec<u8>>> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level)?;
    encoder.window_log(23)?;
    encoder.include_checksum(true)?;
    encoder.set_pledged_src_size(Some(size))?;
    Ok(encoder)
}

fn run(case: &Case, trial: usize, order: usize, candidate: &str, args: &Args) -> Result<Sample> {
    let workspace = tempfile::tempdir_in(&args.output)?;
    let source = workspace.path().join("source");
    let archive = workspace.path().join("archive");
    fs::create_dir_all(&archive)?;
    let mut sample = Sample {
        case: case.name.into(),
        trial,
        order,
        candidate: candidate.into(),
        started_at: Utc::now().to_rfc3339(),
        finished_at: String::new(),
        input_bytes: case.bytes(),
        snapshots: case.count(),
        elapsed_seconds: 0.0,
        input_mib_per_second: 0.0,
        payload_bytes: 0,
        manifest_bytes: 0,
        archive_bytes: 0,
        catalog_bytes: 0,
        total_bytes: 0,
        indexed_total_bytes: None,
        added_index_bytes: None,
        index_seconds: None,
        unique_chunk_bytes: None,
        restored_snapshots: 0,
        errors: 0,
    };
    let mut elapsed = Duration::ZERO;
    if candidate == "nenia" {
        let started = Instant::now();
        let mut vault = Vault::init(&archive, args.level)?;
        elapsed += started.elapsed();
        let mut snapshots = Vec::new();
        for round in &case.rounds {
            let inputs = prepare(&source, round)?;
            let started = Instant::now();
            let report = vault.ingest(&inputs, Source::Auto)?;
            elapsed += started.elapsed();
            ensure!(
                report.archived == round.len() && report.unchanged == 0,
                "unexpected ingestion count"
            );
            snapshots.extend(report.snapshots);
        }
        let stats = vault.stats()?;
        ensure!(
            stats.logical_bytes == case.bytes() && stats.snapshots as usize == case.count(),
            "catalog work mismatch"
        );
        sample.payload_bytes = stats.pack_bytes;
        sample.manifest_bytes = stats.manifest_bytes;
        sample.archive_bytes = stats.archive_bytes;
        sample.unique_chunk_bytes = Some(stats.unique_chunk_bytes);
        let started = Instant::now();
        drop(vault);
        elapsed += started.elapsed();
        sample.total_bytes = tree_bytes(&archive)?;
        sample.catalog_bytes = fs::metadata(archive.join("catalog.sqlite3"))?.len();

        // Correctness is mandatory and deliberately outside the ingestion timer.
        let mut vault = Vault::open(&archive)?;
        ensure!(snapshots.len() == case.count(), "snapshot count mismatch");
        for (id, trace) in snapshots.iter().zip(case.rounds.iter().flatten()) {
            let mut restored = Vec::new();
            vault.export(id, &mut restored)?;
            ensure!(
                restored == trace.bytes,
                "restore mismatch for {}",
                trace.name
            );
            sample.restored_snapshots += 1;
        }
        let started = Instant::now();
        let indexed = vault.index(8 * 1024 * 1024)?;
        drop(vault);
        sample.index_seconds = Some(started.elapsed().as_secs_f64());
        ensure!(indexed == case.count(), "index count mismatch");
        let indexed_total = tree_bytes(&archive)?;
        sample.indexed_total_bytes = Some(indexed_total);
        sample.added_index_bytes = Some(indexed_total.saturating_sub(sample.total_bytes));
    } else {
        let solid = candidate == "solid_zstd";
        let started = Instant::now();
        let mut stream = if solid {
            Some(encoder(case.bytes(), args.level)?)
        } else {
            None
        };
        elapsed += started.elapsed();
        let mut manifest = Vec::new();
        let mut offset = 0_u64;
        for (round_number, round) in case.rounds.iter().enumerate() {
            let inputs = prepare(&source, round)?;
            let started = Instant::now();
            for (i, input) in inputs.iter().enumerate() {
                let object = if solid {
                    "solid.zst".into()
                } else {
                    format!("round-{round_number}-file-{i}.zst")
                };
                let bytes = round[i].bytes.len() as u64;
                let mut file = File::open(&input.path)?;
                if let Some(stream) = stream.as_mut() {
                    io::copy(&mut file, stream)?;
                } else {
                    let mut stream = encoder(bytes, args.level)?;
                    io::copy(&mut file, &mut stream)?;
                    let encoded = stream.finish()?;
                    sample.payload_bytes += encoded.len() as u64;
                    atomic_write_immutable(&archive.join(&object), &encoded)?;
                }
                manifest.push(json!({"round":round_number,"name":round[i].name,"object":object,"offset":if solid {offset} else {0},"bytes":bytes,"hash":blake3::hash(&round[i].bytes).to_hex().to_string()}));
                offset += bytes;
            }
            elapsed += started.elapsed();
        }
        let started = Instant::now();
        if let Some(stream) = stream {
            let encoded = stream.finish()?;
            sample.payload_bytes = encoded.len() as u64;
            atomic_write_immutable(&archive.join("solid.zst"), &encoded)?;
        }
        let manifest = serde_json::to_vec(&manifest)?;
        let mut stream = encoder(manifest.len() as u64, args.level)?;
        stream.write_all(&manifest)?;
        let encoded = stream.finish()?;
        sample.manifest_bytes = encoded.len() as u64;
        atomic_write_immutable(&archive.join("manifest.json.zst"), &encoded)?;
        elapsed += started.elapsed();
        sample.archive_bytes = sample.payload_bytes + sample.manifest_bytes;
        sample.total_bytes = tree_bytes(&archive)?;

        let mapping: Vec<Value> = serde_json::from_slice(&zstd::stream::decode_all(File::open(
            archive.join("manifest.json.zst"),
        )?)?)?;
        let solid_raw = if solid {
            Some(zstd::stream::decode_all(File::open(
                archive.join("solid.zst"),
            )?)?)
        } else {
            None
        };
        ensure!(
            mapping.len() == case.count(),
            "baseline manifest count mismatch"
        );
        for (entry, trace) in mapping.iter().zip(case.rounds.iter().flatten()) {
            let raw;
            let restored = if let Some(solid_raw) = solid_raw.as_ref() {
                let offset = entry["offset"].as_u64().context("missing offset")? as usize;
                &solid_raw[offset..offset + trace.bytes.len()]
            } else {
                raw = zstd::stream::decode_all(File::open(
                    archive.join(entry["object"].as_str().context("missing object")?),
                )?)?;
                &raw[..]
            };
            ensure!(restored == trace.bytes, "baseline restore mismatch");
            sample.restored_snapshots += 1;
        }
    }
    sample.elapsed_seconds = elapsed.as_secs_f64();
    sample.input_mib_per_second = sample.input_bytes as f64 / 1048576.0 / sample.elapsed_seconds;
    sample.finished_at = Utc::now().to_rfc3339();
    Ok(sample)
}

fn command(program: &str, args: &[&str]) -> Value {
    match Command::new(program).args(args).output() {
        Ok(output) => {
            json!({"status":output.status.code(),"stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)})
        }
        Err(error) => json!({"unavailable":error.to_string()}),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.trials > 0 && args.trials <= 100,
        "trials must be 1..100"
    );
    ensure!(
        args.scale.is_finite() && args.scale > 0.0 && args.scale <= 16.0,
        "scale must be >0 and <=16"
    );
    ensure!((-7..=22).contains(&args.level), "invalid Zstd level");
    ensure!(
        !args.output.exists(),
        "output already exists; preserve prior evidence and select a new directory"
    );
    fs::create_dir_all(&args.output)?;
    let mut source_hashes = BTreeMap::new();
    for path in [
        PathBuf::from("Cargo.toml"),
        PathBuf::from("Cargo.lock"),
        PathBuf::from("examples/benchmark.rs"),
    ]
    .into_iter()
    .chain(
        fs::read_dir("src")?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs")),
    ) {
        source_hashes.insert(
            path.to_string_lossy().into_owned(),
            blake3::hash(&fs::read(path)?).to_hex().to_string(),
        );
    }
    let environment = json!({
        "created_at":Utc::now().to_rfc3339(),"command":std::env::args().collect::<Vec<_>>(),
        "seed":format!("{SEED:016x}"),"trials":args.trials,"scale":args.scale,"level":args.level,"zstd_window_log":23,
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,"available_parallelism":std::thread::available_parallelism().ok().map(|n|n.get()),
        "git":command("git", &["rev-parse","HEAD"]),"git_status":command("git", &["status","--short"]),
        "source_blake3":source_hashes,
        "executable_blake3":blake3::hash(&fs::read(std::env::current_exe()?)?).to_hex().to_string(),
        "rustc":command("rustc", &["-Vv"]),"uname":command("uname", &["-a"]),"cpu":command("lscpu", &[]),
        "memory":command("free", &["-b"]),"filesystem":command("df", &["-T",args.output.to_string_lossy().as_ref()]),
        "cgroup_cpu_max":fs::read_to_string("/sys/fs/cgroup/cpu.max").ok(),
        "cgroup_memory_max":fs::read_to_string("/sys/fs/cgroup/memory.max").ok(),
        "limitations":["OS cache not dropped; fresh archive directories, warm source data", "Concurrent development/build work and host interference are uncontrolled", "Short batch throughput, not sustained service capacity", "File lengths, not allocated filesystem blocks or object-store billing", "Power/thermal state and underlying virtual block-device hardware unknown"]
    });
    fs::write(
        args.output.join("environment.json"),
        serde_json::to_vec_pretty(&environment)?,
    )?;
    let cases: Vec<_> = cases(args.scale)
        .into_iter()
        .filter(|case| args.case.as_ref().is_none_or(|name| name == case.name))
        .collect();
    ensure!(!cases.is_empty(), "no matching workload");
    let workloads: Vec<_> = cases.iter().map(|case| json!({"name":case.name,"bytes":case.bytes(),"snapshots":case.count(),"rounds":case.rounds.iter().map(|round| round.iter().map(|trace| json!({"name":trace.name,"bytes":trace.bytes.len(),"blake3":blake3::hash(&trace.bytes).to_hex().to_string()})).collect::<Vec<_>>()).collect::<Vec<_>>()})).collect();
    fs::write(
        args.output.join("workloads.json"),
        serde_json::to_vec_pretty(&workloads)?,
    )?;
    let mut raw = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(args.output.join("samples.jsonl"))?;
    let candidates = ["per_file_zstd", "solid_zstd", "nenia"];
    let mut samples = Vec::new();
    for trial in 0..args.trials {
        for case in &cases {
            for order in 0..candidates.len() {
                let candidate = candidates[(order + trial) % candidates.len()];
                eprintln!(
                    "trial={} case={} candidate={}",
                    trial + 1,
                    case.name,
                    candidate
                );
                match run(case, trial + 1, order + 1, candidate, &args) {
                    Ok(sample) => {
                        serde_json::to_writer(&mut raw, &sample)?;
                        writeln!(raw)?;
                        raw.sync_data()?;
                        samples.push(sample);
                    }
                    Err(error) => {
                        writeln!(
                            raw,
                            "{}",
                            json!({"trial":trial+1,"case":case.name,"candidate":candidate,"error":format!("{error:#}"),"at":Utc::now().to_rfc3339()})
                        )?;
                        raw.sync_data()?;
                        return Err(error);
                    }
                }
            }
        }
    }
    let mut summary = BTreeMap::new();
    for case in &cases {
        for candidate in candidates {
            let rows: Vec<_> = samples
                .iter()
                .filter(|s| s.case == case.name && s.candidate == candidate)
                .collect();
            let mut times: Vec<_> = rows.iter().map(|s| s.elapsed_seconds).collect();
            times.sort_by(f64::total_cmp);
            let median = if times.len() % 2 == 1 {
                times[times.len() / 2]
            } else {
                (times[times.len() / 2 - 1] + times[times.len() / 2]) / 2.0
            };
            summary.insert(format!("{}/{}",case.name,candidate), json!({
                "trials":rows.len(),"elapsed_seconds_min":times[0],"elapsed_seconds_median":median,"elapsed_seconds_max":times[times.len()-1],
                "input_bytes":case.bytes(),"archive_bytes_min":rows.iter().map(|s|s.archive_bytes).min(),"archive_bytes_max":rows.iter().map(|s|s.archive_bytes).max(),
                "total_bytes_min":rows.iter().map(|s|s.total_bytes).min(),"total_bytes_max":rows.iter().map(|s|s.total_bytes).max(),
                "errors":0,"restored_snapshots":rows.iter().map(|s|s.restored_snapshots).sum::<usize>()
            }));
        }
    }
    fs::write(
        args.output.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    Ok(())
}
