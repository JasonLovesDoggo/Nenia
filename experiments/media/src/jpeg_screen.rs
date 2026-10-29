//! Real JPEGs only. Persist and read back Lepton containers, preserving fallbacks.
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, io::Cursor, path::Path, time::Instant};
#[derive(Deserialize)]
struct Asset {
    file: String,
    kind: String,
    cohort: String,
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: jpeg-screen PRIVATE_ASSETS PRIVATE_OUTPUT REPORT"
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
    let features = lepton_jpeg::EnabledFeatures {
        max_threads: 1,
        max_jpeg_width: 8192,
        max_jpeg_height: 8192,
        max_jpeg_file_size: 32 * 1024 * 1024,
        ..lepton_jpeg::EnabledFeatures::compat_lepton_vector_write()
    };
    let pool = lepton_jpeg::SingleThreadPool::default();
    let mut totals = BTreeMap::<String, u64>::new();
    let mut original = None;
    let mut samples = Vec::new();
    let start = Instant::now();
    let mut decode_seconds = 0.;
    for (i, asset) in assets.iter().enumerate() {
        if asset.cohort != "original" && original.is_none() {
            original = Some(totals.clone());
        }
        if asset.kind != "jpeg" {
            continue;
        }
        let data = fs::read(input.join(&asset.file))?;
        let mut payload = zstd::stream::encode_all(data.as_slice(), 9)?;
        let mut mode = 0;
        match lepton_jpeg::encode_lepton_verify(&data, &features, &pool) {
            Ok((candidate, _)) => {
                *totals.entry("lepton_success".into()).or_default() += 1;
                if candidate.len() < payload.len() {
                    payload = candidate;
                    mode = 1;
                }
            }
            Err(e) => {
                *totals
                    .entry(format!("error_{:?}", e.exit_code()))
                    .or_default() += 1;
            }
        }
        let mut container = b"NENIAJP1".to_vec();
        container.push(mode);
        container.extend((data.len() as u32).to_le_bytes());
        container.extend(blake3::hash(&data).as_bytes());
        container.extend(&payload);
        let path = output.join(format!("{i:06}.asset"));
        fs::write(&path, &container)?;
        let restore = Instant::now();
        let stored = fs::read(path)?;
        let mut decoded = Vec::new();
        if stored[8] == 0 {
            decoded = zstd::stream::decode_all(&stored[45..])?;
        } else {
            lepton_jpeg::decode_lepton(
                &mut Cursor::new(&stored[45..]),
                &mut decoded,
                &features,
                &pool,
            )?;
        }
        ensure!(
            decoded == data && blake3::hash(&decoded).as_bytes() == &stored[13..45],
            "JPEG exact reconstruction failed"
        );
        decode_seconds += restore.elapsed().as_secs_f64();
        *totals.entry("images".into()).or_default() += 1;
        *totals.entry("original_bytes".into()).or_default() += data.len() as u64;
        *totals.entry("container_bytes".into()).or_default() += container.len() as u64;
        *totals.entry(format!("selected_mode_{mode}")).or_default() += 1;
        samples.push(serde_json::json!({"ordinal":i,"cohort":asset.cohort,"original_bytes":data.len(),"container_bytes":container.len(),"mode":mode}));
        if totals["images"].is_multiple_of(100) {
            eprintln!(
                "{} exact JPEG restores; {:.1}s",
                totals["images"],
                start.elapsed().as_secs_f64()
            );
        }
    }
    let report = serde_json::json!({"candidate":"Lepton-0.5.5","scope":"Complete independent JPEG asset containers; one exploratory trial; not a trace archive","original":original,"complete":totals,"samples":samples,"wall_seconds":start.elapsed().as_secs_f64(),"decode_seconds":decode_seconds});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}",
        serde_json::json!({"totals":totals,"wall_seconds":start.elapsed().as_secs_f64()})
    );
    Ok(())
}
