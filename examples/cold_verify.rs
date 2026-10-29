//! Independent bytewise restore oracle and real HTTP replication of cold vaults.
use anyhow::{ensure, Context, Result};
use nenia::{
    remote::{self, SyncOptions},
    server,
    vault::Vault,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};
#[derive(Deserialize)]
struct Corpus {
    files: Vec<Input>,
}
#[derive(Deserialize)]
struct Input {
    copy: PathBuf,
    original: PathBuf,
    hash: String,
    bytes: u64,
}
struct Oracle {
    source: File,
    bytes: u64,
}
impl Write for Oracle {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut buffer = [0u8; 65536];
        for part in data.chunks(buffer.len()) {
            self.source.read_exact(&mut buffer[..part.len()])?;
            if &buffer[..part.len()] != part {
                return Err(std::io::Error::other("source byte mismatch"));
            }
        }
        self.bytes += data.len() as u64;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn verify(
    root: &Path,
    corpus: &HashMap<(String, String), (PathBuf, u64)>,
) -> Result<serde_json::Value> {
    let start = Instant::now();
    let vault = Vault::open(root)?;
    let snapshots = vault.list(None, 100_000, true)?;
    ensure!(
        !snapshots.is_empty(),
        "byte oracle requires a populated catalog"
    );
    let mut bytes = 0;
    let mut native = 0;
    for (i, snapshot) in snapshots.iter().enumerate() {
        let (source, size) = corpus
            .get(&(snapshot.origin.clone(), snapshot.content_hash.clone()))
            .context("snapshot absent from frozen corpus")?;
        let mut oracle = Oracle {
            source: File::open(source)?,
            bytes: 0,
        };
        vault.export(&snapshot.id, &mut oracle)?;
        ensure!(
            oracle.bytes == *size && oracle.source.read(&mut [0])? == 0,
            "source length mismatch"
        );
        bytes += oracle.bytes;
        if source.extension().is_some_and(|s| s == "zst") {
            native += 1;
        }
        if (i + 1).is_multiple_of(100) {
            eprintln!("{} exact source comparisons", i + 1);
        }
    }
    Ok(
        serde_json::json!({"files":snapshots.len(),"bytes":bytes,"native_zstd_files":native,"seconds":start.elapsed().as_secs_f64(),"comparison":"literal byte comparison against frozen original files, including final EOF"}),
    )
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        (4..=5).contains(&args.len()),
        "usage: cold_verify VAULT CORPUS REPORT [http]"
    );
    ensure!(!Path::new(&args[3]).exists(), "report exists");
    let corpus: Corpus = serde_json::from_slice(&fs::read(&args[2])?)?;
    let corpus = corpus
        .files
        .into_iter()
        .map(|i| {
            (
                (i.original.to_string_lossy().into_owned(), i.hash),
                (i.copy, i.bytes),
            )
        })
        .collect::<HashMap<_, _>>();
    let root = Path::new(&args[1]);
    let original = verify(root, &corpus)?;
    let mut report = serde_json::json!({"original":original});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    if args.get(4).is_some_and(|s| s == "http") {
        let work = tempfile::tempdir_in(root.parent().context("vault parent")?)?;
        let server_root = work.path().join("server");
        let receiver = work.path().join("receiver");
        let mut entropy = [0; 32];
        getrandom::fill(&mut entropy).map_err(|e| anyhow::anyhow!("benchmark token: {e}"))?;
        let token = blake3::hash(&entropy).to_hex().to_string();
        let app = server::router(Vault::init(&server_root, 9)?, token.clone())?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        let options = SyncOptions {
            token: Some(token),
            ..Default::default()
        };
        let endpoint = format!("http://{address}");
        let start = Instant::now();
        let pushed = remote::push(root, &endpoint, &options).await?;
        drop(Vault::init(&receiver, 9)?);
        let pulled = remote::pull(&receiver, &endpoint, &options).await?;
        let transferred_seconds = start.elapsed().as_secs_f64();
        let _ = stop.send(());
        task.await??;
        // pull transfers immutable objects; the CLI rebuilds the disposable
        // catalog before listing or restoring them. An empty list is not proof.
        Vault::open(&receiver)?.rebuild()?;
        let replica = verify(&receiver, &corpus)?;
        ensure!(
            replica["files"] == original["files"] && replica["bytes"] == original["bytes"],
            "HTTP replica coverage differs"
        );
        let snapshots = Vault::rebuild_catalog(&receiver)?;
        let rebuilt = verify(&receiver, &corpus)?;
        ensure!(
            rebuilt["files"] == original["files"] && rebuilt["bytes"] == original["bytes"],
            "catalog recovery coverage differs"
        );
        report["http"] = serde_json::json!({"push":pushed,"pull":pulled,"transfer_seconds":transferred_seconds,"replica":replica,"rebuilt_snapshots":snapshots,"rebuilt":rebuilt});
    }
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    Ok(())
}
