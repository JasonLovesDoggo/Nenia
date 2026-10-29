//! Translation-aware screenshot deltas. References always point to keyframes:
//! every image requires at most one other image, never an unbounded delta chain.
use anyhow::{Result, ensure};
use nenia_media_screen::png;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Read,
    path::Path,
    time::Instant,
};
#[derive(Deserialize)]
struct Asset {
    file: String,
    kind: String,
    cohort: String,
}
#[derive(Serialize, Deserialize)]
struct Frame {
    width: usize,
    height: usize,
    channels: usize,
    parent: Option<usize>,
    dy: i32,
    subtract: bool,
    codec: u8,
    hash: String,
}
struct Base {
    id: usize,
    width: usize,
    height: usize,
    channels: usize,
    pixels: Vec<u8>,
    rows: HashMap<u64, Vec<usize>>,
}
fn rows(pixels: &[u8], width: usize, channels: usize) -> HashMap<u64, Vec<usize>> {
    let mut map = HashMap::<u64, Vec<usize>>::new();
    for (y, row) in pixels.chunks_exact(width * channels).enumerate() {
        if row.chunks_exact(channels).all(|p| p == &row[..channels]) {
            continue;
        }
        let key = u64::from_le_bytes(blake3::hash(row).as_bytes()[..8].try_into().unwrap());
        let ys = map.entry(key).or_default();
        if ys.len() < 4 {
            ys.push(y);
        }
    }
    map
}
fn encode(pixels: &[u8], width: usize, height: usize, channels: usize) -> Result<(u8, Vec<u8>)> {
    let raw = zstd::stream::encode_all(pixels, 9)?;
    if matches!(channels, 3 | 4) {
        let webp = png::webp_encode(pixels, width, height, channels)?;
        if webp.len() < raw.len() {
            return Ok((1, webp));
        }
    }
    Ok((0, raw))
}
fn residual(
    pixels: &[u8],
    width: usize,
    height: usize,
    channels: usize,
    base: &Base,
    dy: i32,
    subtract: bool,
) -> Vec<u8> {
    let row = width * channels;
    let mut out = pixels.to_vec();
    for y in 0..height {
        let source = y as i32 + dy;
        if source >= 0 && (source as usize) < base.height {
            let source = source as usize;
            for x in 0..row {
                let v = base.pixels[source * row + x];
                out[y * row + x] = if subtract {
                    pixels[y * row + x].wrapping_sub(v)
                } else {
                    pixels[y * row + x] ^ v
                };
            }
        }
    }
    out
}
fn restore(root: &Path, id: usize, depth: usize) -> Result<(Frame, Vec<u8>)> {
    ensure!(depth <= 1, "reference depth");
    let bytes = fs::read(root.join(format!("{id:06}.frame")))?;
    ensure!(
        bytes.len() >= 12 && &bytes[..8] == b"NENIATM1",
        "frame header"
    );
    let n = u32::from_le_bytes(bytes[8..12].try_into()?) as usize;
    ensure!(n <= 1024 && 12 + n <= bytes.len(), "frame metadata size");
    let frame: Frame = serde_json::from_slice(&bytes[12..12 + n])?;
    let payload = &bytes[12 + n..];
    ensure!(
        frame.width > 0
            && frame.height > 0
            && frame.width <= 16384
            && frame.height <= 16384
            && (1..=4).contains(&frame.channels)
            && frame.width * frame.height * frame.channels <= 128 * 1024 * 1024,
        "frame pixel budget"
    );
    let mut pixels = match frame.codec {
        0 => {
            let mut d = zstd::stream::read::Decoder::new(payload)?;
            d.window_log_max(27)?;
            let mut out = Vec::new();
            d.take(128 * 1024 * 1024 + 1).read_to_end(&mut out)?;
            out
        }
        1 => png::webp_decode(payload, frame.width, frame.height, frame.channels)?,
        _ => anyhow::bail!("frame codec"),
    };
    ensure!(
        pixels.len() == frame.width * frame.height * frame.channels,
        "frame pixel size"
    );
    if let Some(parent) = frame.parent {
        ensure!(parent < id && depth == 0, "frame graph");
        let (base, base_pixels) = restore(root, parent, depth + 1)?;
        ensure!(
            base.parent.is_none() && base.width == frame.width && base.channels == frame.channels,
            "reference layout"
        );
        let row = frame.width * frame.channels;
        for y in 0..frame.height {
            let source = y as i32 + frame.dy;
            if source >= 0 && (source as usize) < base.height {
                for x in 0..row {
                    let v = base_pixels[source as usize * row + x];
                    pixels[y * row + x] = if frame.subtract {
                        pixels[y * row + x].wrapping_add(v)
                    } else {
                        pixels[y * row + x] ^ v
                    };
                }
            }
        }
    }
    ensure!(
        blake3::hash(&pixels).to_hex().as_str() == frame.hash,
        "frame hash"
    );
    Ok((frame, pixels))
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: temporal PRIVATE_ASSETS PRIVATE_OUTPUT REPORT"
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
    let mut bases = VecDeque::<Base>::new();
    let mut samples = Vec::new();
    let mut total = 0;
    let mut original = None;
    let mut deltas = 0;
    let mut skipped = 0;
    let mut decode_seconds = 0.;
    let start = Instant::now();
    for (id, asset) in assets.iter().enumerate() {
        if asset.cohort != "original" && original.is_none() {
            original = Some(
                serde_json::json!({"images":samples.len(),"container_bytes":total,"deltas":deltas}),
            );
        }
        if asset.kind != "png" {
            continue;
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
        let features = rows(&img.pixels, img.width, img.channels);
        let (codec, mut payload) = encode(&img.pixels, img.width, img.height, img.channels)?;
        let independent_bytes = payload.len();
        let mut frame = Frame {
            width: img.width,
            height: img.height,
            channels: img.channels,
            parent: None,
            dy: 0,
            subtract: false,
            codec,
            hash: blake3::hash(&img.pixels).to_hex().to_string(),
        };
        let mut candidates = Vec::new();
        for (position, base) in bases.iter().enumerate() {
            if base.width != img.width || base.channels != img.channels {
                continue;
            }
            let mut votes = HashMap::<i32, usize>::new();
            votes.insert(0, 0);
            for (hash, ys) in &features {
                if let Some(by) = base.rows.get(hash) {
                    for &y in ys {
                        for &b in by {
                            *votes.entry(b as i32 - y as i32).or_default() += 1;
                        }
                    }
                }
            }
            let (&dy, &score) = votes
                .iter()
                .max_by_key(|(dy, n)| (**n, std::cmp::Reverse(dy.abs())))
                .unwrap();
            candidates.push((score, base.id, position, dy));
        }
        candidates.sort_unstable_by(|a, b| b.cmp(a));
        for &(_, _, position, dy) in candidates.iter().take(2) {
            let base = &bases[position];
            for subtract in [false, true] {
                let diff = residual(
                    &img.pixels,
                    img.width,
                    img.height,
                    img.channels,
                    base,
                    dy,
                    subtract,
                );
                let (codec, candidate) = encode(&diff, img.width, img.height, img.channels)?;
                // The parent ID costs at most 20 bytes more in serialized metadata.
                if candidate.len() + 20 < payload.len() {
                    payload = candidate;
                    frame.parent = Some(base.id);
                    frame.dy = dy;
                    frame.subtract = subtract;
                    frame.codec = codec;
                }
            }
        }
        let metadata = serde_json::to_vec(&frame)?;
        let mut container = b"NENIATM1".to_vec();
        container.extend((metadata.len() as u32).to_le_bytes());
        container.extend(metadata);
        container.extend(payload);
        fs::write(output.join(format!("{id:06}.frame")), &container)?;
        let restore_start = Instant::now();
        let (_, decoded) = restore(output, id, 0)?;
        ensure!(
            decoded == img.pixels,
            "exact screenshot reconstruction failed"
        );
        decode_seconds += restore_start.elapsed().as_secs_f64();
        total += container.len() as u64;
        let delta = frame.parent.is_some();
        if delta {
            deltas += 1;
        }
        samples.push(serde_json::json!({"ordinal":id,"cohort":asset.cohort,"container_bytes":container.len(),"independent_payload_bytes":independent_bytes,"delta":delta,"scroll_rows":frame.dy,"subtract":frame.subtract}));
        if !delta {
            while !bases.is_empty()
                && bases.iter().map(|b| b.pixels.len()).sum::<usize>() + img.pixels.len()
                    > 128 * 1024 * 1024
            {
                bases.pop_back();
            }
            bases.push_front(Base {
                id,
                width: img.width,
                height: img.height,
                channels: img.channels,
                pixels: img.pixels,
                rows: features,
            });
        }
        if samples.len().is_multiple_of(50) {
            eprintln!(
                "{} frames; {deltas} deltas; {:.1}s",
                samples.len(),
                start.elapsed().as_secs_f64()
            );
        }
    }
    let report = serde_json::json!({"candidate":"scroll-aligned-keyframe-deltas","scope":"Complete pixel-only containers; PNG reconstruction corrections excluded; one exploratory trial","original":original,"images":samples.len(),"skipped_images":skipped,"container_bytes":total,"delta_images":deltas,"max_reference_depth":1,"encoder_reference_pixel_budget":128*1024*1024,"samples":samples,"wall_seconds":start.elapsed().as_secs_f64(),"restore_seconds":decode_seconds});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}",
        serde_json::json!({"images":samples.len(),"bytes":total,"deltas":deltas,"wall_seconds":start.elapsed().as_secs_f64()})
    );
    Ok(())
}
