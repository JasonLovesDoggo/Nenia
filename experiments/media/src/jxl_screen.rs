//! Lossless JPEG XL control on actual pixels, including invisible RGB.
use anyhow::{Result, ensure};
use nenia_media_screen::png;
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs,
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};
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
        args.len() == 7,
        "usage: jxl-screen PRIVATE_ASSETS PRIVATE_OUTPUT REPORT CJXL original|all|sample EFFORT"
    );
    let input = Path::new(&args[1]);
    let out = Path::new(&args[2]);
    ensure!(!out.exists(), "output exists");
    fs::create_dir_all(out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(out, fs::Permissions::from_mode(0o700))?;
    }
    let assets: Vec<Asset> = serde_json::from_slice(&fs::read(input.join("assets.json"))?)?;
    let mut selected = assets
        .iter()
        .enumerate()
        .filter(|(_, a)| a.kind == "png" && (args[5] == "all" || a.cohort == "original"))
        .map(|(id, a)| (id, a.bytes))
        .collect::<Vec<_>>();
    if args[5] == "sample" {
        let mut by_size = selected.clone();
        by_size.sort_by_key(|&(id, n)| std::cmp::Reverse((n, id)));
        let mut ids = by_size
            .iter()
            .take(10)
            .map(|&(id, _)| id)
            .collect::<HashSet<_>>();
        for j in 0..20 {
            ids.insert(selected[j * selected.len() / 20].0);
        }
        selected.retain(|(id, _)| ids.contains(id));
    } else {
        ensure!(args[5] == "all" || args[5] == "original", "cohort");
    }
    let start = Instant::now();
    let mut samples = Vec::new();
    let mut total = 0u64;
    let mut independent = 0u64;
    let mut jxl_total = 0u64;
    let mut original = None;
    let mut winners = 0;
    let mut skipped = 0;
    let mut decode_seconds = 0.;
    let temp = tempfile::tempdir_in(out)?;
    for (id, _) in selected {
        let asset = &assets[id];
        if asset.cohort != "original" && original.is_none() {
            original = Some(
                serde_json::json!({"images":samples.len(),"container_bytes":total,"independent_payload_bytes":independent,"jxl_payload_bytes":jxl_total,"selected_jxl":winners}),
            );
        }
        let img = match png::parse(&fs::read(input.join(&asset.file))?) {
            Ok(v) => v,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if img.depth != 8 {
            skipped += 1;
            continue;
        }
        let kind = match img.channels {
            1 => "GRAYSCALE",
            2 => "GRAYSCALE_ALPHA",
            3 => "RGB",
            4 => "RGB_ALPHA",
            _ => unreachable!(),
        };
        let mut pam = format!(
            "P7\nWIDTH {}\nHEIGHT {}\nDEPTH {}\nMAXVAL 255\nTUPLTYPE {}\nENDHDR\n",
            img.width, img.height, img.channels, kind
        )
        .into_bytes();
        pam.extend_from_slice(&img.pixels);
        let source = temp.path().join("input.pam");
        let target = temp.path().join("output.jxl");
        fs::write(&source, pam)?;
        let status = Command::new(&args[4])
            .arg(&source)
            .arg(&target)
            .args([
                "-d",
                "0",
                "-e",
                &args[6],
                "--num_threads=0",
                "--keep_invisible=1",
                "--quiet",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        ensure!(
            status.status.success(),
            "cjxl failed: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        let encoded = fs::read(&target)?;
        let restore = Instant::now();
        ensure!(
            png::jxl_decode(&encoded, img.width, img.height, img.channels)? == img.pixels,
            "JXL changed exact pixel samples"
        );
        decode_seconds += restore.elapsed().as_secs_f64();
        let mut payload = zstd::stream::encode_all(img.pixels.as_slice(), 9)?;
        let mut codec = 0;
        if matches!(img.channels, 3 | 4) {
            let candidate = png::webp_encode(&img.pixels, img.width, img.height, img.channels)?;
            if candidate.len() < payload.len() {
                payload = candidate;
                codec = 1;
            }
        }
        independent += payload.len() as u64;
        jxl_total += encoded.len() as u64;
        let before = payload.len();
        let jxl = encoded.len();
        if encoded.len() < payload.len() {
            payload = encoded;
            codec = 2;
            winners += 1;
        }
        let metadata = serde_json::to_vec(
            &serde_json::json!({"width":img.width,"height":img.height,"channels":img.channels,"parent":null,"dy":0,"subtract":false,"codec":codec,"hash":blake3::hash(&img.pixels).to_hex().to_string()}),
        )?;
        let mut frame = b"NENIATM1".to_vec();
        frame.extend((metadata.len() as u32).to_le_bytes());
        frame.extend(metadata);
        frame.extend(payload);
        fs::write(out.join(format!("{id:06}.frame")), &frame)?;
        total += frame.len() as u64;
        samples.push(serde_json::json!({"ordinal":id,"cohort":asset.cohort,"pixels_bytes":img.pixels.len(),"independent_payload_bytes":before,"jxl_payload_bytes":jxl,"selected_codec":codec,"container_bytes":frame.len()}));
        if samples.len().is_multiple_of(10) {
            eprintln!(
                "{} exact JXL pixel restores; {winners} selected; {:.1}s",
                samples.len(),
                start.elapsed().as_secs_f64()
            );
        }
    }
    let report = serde_json::json!({"candidate":"lossless-JXL-0.11.2-with-pure-Rust-0.12.6-decoder","scope":"Pixel-only control including per-frame metadata; PNG reconstruction excluded; one exploratory trial","selection":args[5],"effort":args[6],"original":original,"images":samples.len(),"skipped_images":skipped,"container_bytes":total,"independent_payload_bytes":independent,"jxl_payload_bytes":jxl_total,"selected_jxl":winners,"samples":samples,"wall_seconds":start.elapsed().as_secs_f64(),"decode_seconds":decode_seconds});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}",
        serde_json::json!({"images":samples.len(),"selected_jxl":winners,"bytes":total,"independent_pixels":independent,"jxl_pixels":jxl_total})
    );
    Ok(())
}
