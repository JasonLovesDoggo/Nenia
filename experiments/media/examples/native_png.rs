//! Exact historical zlib profile screen. No images or identifiers in reports.
use anyhow::Result;
use nenia_media_screen::png;
use std::{collections::BTreeMap, fs, path::Path, time::Instant};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let root = Path::new(&args[1]);
    let assets: Vec<serde_json::Value> =
        serde_json::from_slice(&fs::read(root.join("assets.json"))?)?;
    let mut stats = BTreeMap::<String, u64>::new();
    let mut samples = Vec::new();
    let start = Instant::now();
    for (i, a) in assets.iter().enumerate() {
        if a["kind"] != "png" {
            continue;
        }
        let Ok(img) = png::parse(&fs::read(root.join(a["file"].as_str().unwrap()))?) else {
            continue;
        };
        let stored = fs::read(Path::new(&args[2]).join(format!("{i:06}.asset")))?;
        if stored[8] == 1 && matches!(stored[45], 2 | 3) {
            *stats.entry("already_exact_fdeflate".into()).or_default() += 1;
            continue;
        }
        let raw = png::refilter(
            &img.pixels,
            &img.filters,
            (img.width * img.depth * img.channels).div_ceil(8),
            (img.depth * img.channels).div_ceil(8),
        )?;
        let levels: Vec<i32> = match img.idat[1] >> 6 {
            0 => vec![1, 0],
            1 => vec![3, 2],
            2 => vec![6, 5, 4],
            _ => vec![9, 8, 7],
        };
        let mut profile = None;
        let mut attempts = 0;
        let mut buffer = vec![0; img.idat.len() + 64];
        'search: for strategy in [
            zlib_rs::Strategy::Default,
            zlib_rs::Strategy::Filtered,
            zlib_rs::Strategy::Rle,
            zlib_rs::Strategy::HuffmanOnly,
            zlib_rs::Strategy::Fixed,
        ] {
            if img.idat[1] >> 6 != 0
                && matches!(
                    strategy,
                    zlib_rs::Strategy::Rle
                        | zlib_rs::Strategy::HuffmanOnly
                        | zlib_rs::Strategy::Fixed
                )
            {
                continue;
            }
            for &level in &levels {
                for mem_level in [8, 9] {
                    attempts += 1;
                    let (encoded, _) = zlib_rs::compress_slice(
                        &mut buffer,
                        &raw,
                        zlib_rs::DeflateConfig {
                            level,
                            mem_level,
                            window_bits: 15,
                            strategy,
                            ..Default::default()
                        },
                    );
                    if encoded == img.idat {
                        profile = Some(format!(
                            "zlib-rs-0.6.8 level={level} memory={mem_level} strategy={strategy:?}"
                        ));
                        break 'search;
                    }
                }
            }
        }
        *stats
            .entry(profile.clone().unwrap_or_else(|| "no_exact_profile".into()))
            .or_default() += 1;
        samples.push(serde_json::json!({"ordinal":i,"cohort":a["cohort"],"source_zlib_bytes":img.idat.len(),"attempts":attempts,"exact_profile":profile}));
        if samples.len().is_multiple_of(50) {
            eprintln!(
                "{} PNG native-profile proofs attempted; {:.1}s",
                samples.len(),
                start.elapsed().as_secs_f64()
            );
        }
    }
    let report = serde_json::json!({"candidate":"native-zlib-rs-profile-proof","stats":stats,"samples":samples,"seconds":start.elapsed().as_secs_f64()});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string(&stats)?);
    Ok(())
}
