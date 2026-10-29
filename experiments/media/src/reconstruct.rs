//! Complete private asset containers, always verified against original bytes.
mod deflate_recipe;
mod png;
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::Path, time::Instant};

#[derive(Deserialize)]
struct Asset {
    file: String,
    kind: String,
    cohort: String,
    bytes: u64,
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() >= 4,
        "usage: reconstruct PRIVATE_ASSETS PRIVATE_OUTPUT AGGREGATE_REPORT [limit]"
    );
    let input = Path::new(&args[1]);
    let output = Path::new(&args[2]);
    ensure!(!output.exists(), "output exists");
    fs::create_dir_all(output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(output, fs::Permissions::from_mode(0o700))?;
    }
    let assets: Vec<Asset> = serde_json::from_slice(&fs::read(input.join("assets.json"))?)?;
    let limit = args
        .get(4)
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(usize::MAX);
    let mut totals = BTreeMap::<String, u64>::new();
    let mut samples = Vec::new();
    let mut original = None;
    let start = Instant::now();
    let mut decode_seconds = 0.;
    for (i, asset) in assets.iter().enumerate().take(limit) {
        if asset.cohort != "original" && original.is_none() {
            original = Some(totals.clone());
        }
        if asset.kind != "png" {
            continue;
        }
        let data = fs::read(input.join(&asset.file))?;
        ensure!(data.len() as u64 == asset.bytes, "asset size changed");
        let (container, stats) = png::encode(&data)?;
        let path = output.join(format!("{i:06}.asset"));
        fs::write(&path, &container)?;
        let restore = Instant::now();
        let restored = png::decode(&fs::read(path)?)?;
        ensure!(
            restored == data,
            "asset reconstruction mismatch at ordinal {i}"
        );
        decode_seconds += restore.elapsed().as_secs_f64();
        *totals.entry("images".into()).or_default() += 1;
        *totals.entry("original_bytes".into()).or_default() += data.len() as u64;
        *totals.entry("container_bytes".into()).or_default() += container.len() as u64;
        for (key, value) in &stats {
            *totals.entry(key.clone()).or_default() += value;
        }
        samples.push(serde_json::json!({"ordinal":i,"cohort":asset.cohort,"original_bytes":data.len(),"container_bytes":container.len(),"stats":stats}));
        if i % 50 == 0 {
            eprintln!(
                "{i} assets; {:.1}s; {} exact PNG restores",
                start.elapsed().as_secs_f64(),
                totals["images"]
            );
        }
    }
    let report = serde_json::json!({"candidate":"exact-independent-PNG-containers","scope":"Complete PNG asset containers; not a trace archive; one exploratory trial","original":original.unwrap_or_else(||totals.clone()),"complete":totals,"samples":samples,"wall_seconds":start.elapsed().as_secs_f64(),"restore_seconds":decode_seconds,"private_content_published":false});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}",
        serde_json::json!({"totals":totals,"wall_seconds":start.elapsed().as_secs_f64()})
    );
    Ok(())
}
