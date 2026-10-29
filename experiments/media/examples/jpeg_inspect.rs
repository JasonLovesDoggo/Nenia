use anyhow::Result;
use nenia_media_screen::jpeg;
use std::{collections::BTreeMap, fs, path::Path};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let root = Path::new(&args[1]);
    let assets: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(root.join("assets.json"))?)?;
    let mut report = BTreeMap::<String, (usize, usize)>::new();
    for a in assets {
        if a["kind"] != "jpeg" {
            continue;
        }
        let bytes = fs::read(root.join(a["file"].as_str().unwrap()))?;
        let outcome = jpeg::decode(&bytes).and_then(|v| jpeg::encode(&v));
        let reason = match outcome {
            Ok(v) if v == bytes => "exact".into(),
            Ok(_) => "noncanonical entropy encoding".into(),
            Err(e) => e.to_string(),
        };
        let key = format!("{}: {reason}", a["cohort"].as_str().unwrap());
        let stats = report.entry(key).or_default();
        stats.0 += 1;
        stats.1 += bytes.len();
    }
    let report = serde_json::to_string_pretty(&report)?;
    fs::write(&args[2], &report)?;
    println!("{report}");
    Ok(())
}
