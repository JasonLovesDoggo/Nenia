//! Exact screenshot sprites: background colors, connected residual shapes,
//! polarity normalization, and cross-image sharing. No OCR or pixel quantization.
use anyhow::{Result, ensure};
use nenia_media_screen::{
    pack, png,
    shared_pixels::{Packed, Reader, Recipe},
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    time::Instant,
};
const TILE: usize = 32;
type Key = (usize, usize, usize, blake3::Hash);
#[derive(Deserialize)]
struct Asset {
    file: String,
    kind: String,
    cohort: String,
}
#[derive(Clone, Copy)]
struct Run {
    y: u32,
    start: u32,
    end: u32,
}
struct Shape {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    channels: usize,
    pixels: Vec<u8>,
    invert: u32,
}
fn color(p: &[u8]) -> u32 {
    let mut bytes = [0; 4];
    bytes[..p.len()].copy_from_slice(p);
    u32::from_le_bytes(bytes)
}
fn root(parent: &mut [usize], mut i: usize) -> usize {
    let mut r = i;
    while parent[r] != r {
        r = parent[r];
    }
    while parent[i] != i {
        let next = parent[i];
        parent[i] = r;
        i = next;
    }
    r
}
type Segmentation = (Vec<u32>, Vec<Shape>, Option<Vec<u8>>);
fn prepare(img: &png::Image, residual: bool) -> Result<Segmentation> {
    let (w, h, c) = (img.width, img.height, img.channels);
    let count = w * h;
    let cols = w.div_ceil(TILE);
    let mut palette = HashMap::<u32, usize>::new();
    for i in (0..count).step_by((count / 4096).max(1)) {
        *palette
            .entry(color(&img.pixels[i * c..(i + 1) * c]))
            .or_default() += 1;
    }
    let (&global, _) = palette
        .iter()
        .max_by_key(|(color, n)| (**n, std::cmp::Reverse(**color)))
        .unwrap();
    let mut background = Vec::new();
    for y in (0..h).step_by(TILE) {
        for x in (0..w).step_by(TILE) {
            let right = (x + TILE).min(w);
            let bottom = (y + TILE).min(h);
            if residual {
                let mut hist = [[0u16; 256]; 4];
                for yy in y..bottom {
                    for xx in x..right {
                        for cc in 0..c {
                            hist[cc][img.pixels[(yy * w + xx) * c + cc] as usize] += 1;
                        }
                    }
                }
                let mut median = [0u8; 4];
                let target = ((right - x) * (bottom - y) / 2).max(1);
                for cc in 0..c {
                    let mut count = 0;
                    for (value, &frequency) in hist[cc].iter().enumerate() {
                        count += frequency as usize;
                        if count >= target {
                            median[cc] = value as u8;
                            break;
                        }
                    }
                }
                background.push(u32::from_le_bytes(median));
                continue;
            }
            let points = [
                (x, y),
                (right - 1, y),
                (x, bottom - 1),
                (right - 1, bottom - 1),
                ((x + right) / 2, (y + bottom) / 2),
            ];
            let mut candidates = vec![global];
            for (xx, yy) in points {
                let v = color(&img.pixels[(yy * w + xx) * c..(yy * w + xx + 1) * c]);
                if !candidates.contains(&v) {
                    candidates.push(v);
                }
            }
            let mut counts = vec![0; candidates.len()];
            for yy in y..bottom {
                for xx in x..right {
                    let value = color(&img.pixels[(yy * w + xx) * c..(yy * w + xx + 1) * c]);
                    for (i, &candidate) in candidates.iter().enumerate() {
                        if candidate == value {
                            counts[i] += 1;
                        }
                    }
                }
            }
            let i = (0..counts.len())
                .max_by_key(|&i| (counts[i], std::cmp::Reverse(i)))
                .unwrap();
            background.push(if counts[i] * 2 >= (right - x) * (bottom - y) {
                candidates[i]
            } else {
                global
            });
        }
    }
    let foreground = |x: usize, y: usize| {
        let value = &img.pixels[(y * w + x) * c..(y * w + x + 1) * c];
        let bg = background[y / TILE * cols + x / TILE].to_le_bytes();
        if residual {
            value.iter().zip(bg).any(|(&v, b)| v.abs_diff(b) > 8)
        } else {
            value != &bg[..c]
        }
    };
    let mut base = residual.then(|| img.pixels.clone());
    let mut runs = Vec::<Run>::new();
    let mut parent = Vec::new();
    let mut previous = 0..0;
    for y in 0..h {
        let start = runs.len();
        let mut prior = previous.start;
        let mut x = 0;
        while x < w {
            if !foreground(x, y) {
                x += 1;
                continue;
            }
            let begin = x;
            x += 1;
            while x < w && foreground(x, y) {
                x += 1;
            }
            ensure!(runs.len() < 1_000_000, "sprite run budget");
            let id = runs.len();
            parent.push(id);
            runs.push(Run {
                y: y as u32,
                start: begin as u32,
                end: x as u32,
            });
            while prior < previous.end && (runs[prior].end as usize) < begin {
                prior += 1;
            }
            let mut p = prior;
            while p < previous.end && runs[p].start as usize <= x {
                let a = root(&mut parent, id);
                let b = root(&mut parent, p);
                parent[a.max(b)] = a.min(b);
                p += 1;
            }
        }
        previous = start..runs.len();
    }
    let mut bounds = vec![[u32::MAX, u32::MAX, 0, 0]; runs.len()];
    let mut ids = Vec::with_capacity(runs.len());
    for (i, run) in runs.iter().enumerate() {
        let r = root(&mut parent, i);
        ids.push((r, i));
        let b = &mut bounds[r];
        b[0] = b[0].min(run.start);
        b[1] = b[1].min(run.y);
        b[2] = b[2].max(run.end);
        b[3] = b[3].max(run.y + 1);
    }
    ids.sort_unstable();
    let channels = if c == 4 && img.pixels.chunks_exact(4).all(|p| p[3] == 255) {
        3
    } else {
        c
    };
    let mut shapes = Vec::new();
    let mut work = 0;
    let mut pos = 0;
    while pos < ids.len() {
        ensure!(shapes.len() < 100_000, "sprite component budget");
        let label = ids[pos].0;
        let [x, y, right, bottom] = bounds[label];
        let (x, y, sw, sh) = (
            x as usize,
            y as usize,
            (right - x) as usize,
            (bottom - y) as usize,
        );
        work += sw * sh * c;
        ensure!(work <= img.pixels.len() * 4, "sprite area budget");
        let mut pixels = vec![0; sw * sh * channels];
        while pos < ids.len() && ids[pos].0 == label {
            let run = runs[ids[pos].1];
            let yy = run.y as usize;
            for xx in run.start as usize..run.end as usize {
                let bg = background[yy / TILE * cols + xx / TILE].to_le_bytes();
                if let Some(base) = &mut base {
                    base[(yy * w + xx) * c..(yy * w + xx + 1) * c].copy_from_slice(&bg[..c]);
                }
                for cc in 0..channels {
                    pixels[((yy - y) * sw + xx - x) * channels + cc] =
                        img.pixels[(yy * w + xx) * c + cc].wrapping_sub(bg[cc]);
                }
            }
            pos += 1;
        }
        let invert = u32::from(
            pixels
                .iter()
                .find(|&&v| v != 0 && v != 128)
                .is_some_and(|&v| v > 128),
        );
        if invert != 0 {
            for v in &mut pixels {
                *v = 0u8.wrapping_sub(*v);
            }
        }
        shapes.push(Shape {
            x,
            y,
            width: sw,
            height: sh,
            channels,
            pixels,
            invert,
        });
    }
    Ok((background, shapes, base))
}
fn encode(raw: &[u8], width: usize, height: usize, channels: usize) -> Result<(Packed, Vec<u8>)> {
    let mut bytes = zstd::stream::encode_all(raw, 9)?;
    let mut codec = 0;
    if raw.len() >= 1024 && matches!(channels, 3 | 4) && width <= 16383 && height <= 16383 {
        let webp = png::webp_encode(raw, width, height, channels)?;
        if webp.len() < bytes.len() {
            bytes = webp;
            codec = 1;
        }
    }
    let packed = Packed {
        width,
        height,
        channels,
        codec,
        hash: blake3::hash(&bytes).to_hex().to_string(),
        storage: Vec::new(),
    };
    Ok((packed, bytes))
}
fn token(recipe: &Recipe) -> Result<Vec<u8>> {
    Ok(zstd::stream::encode_all(
        serde_json::to_vec(recipe)?.as_slice(),
        9,
    )?)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        (4..=6).contains(&args.len()),
        "usage: sprites PRIVATE_ASSETS PRIVATE_OUTPUT REPORT [original|all] [residual]"
    );
    let residual = args.get(5).is_some_and(|s| s == "residual");
    let input = Path::new(&args[1]);
    let out = Path::new(&args[2]);
    ensure!(!out.exists(), "output exists");
    fs::create_dir_all(out.join("objects"))?;
    fs::create_dir(out.join("recipes"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(out, fs::Permissions::from_mode(0o700))?;
    }
    let assets: Vec<Asset> = serde_json::from_slice(&fs::read(input.join("assets.json"))?)?;
    let mut known = HashMap::<Key, Packed>::new();
    let mut objects = HashSet::<String>::new();
    let mut reader = Reader::new(out.join("objects"));
    let mut samples = Vec::new();
    let mut original = None;
    let mut object_bytes = 0u64;
    let mut recipe_bytes = 0u64;
    let mut selected = 0;
    let mut skipped = 0;
    let mut rejected = 0;
    let mut restore_seconds = 0.;
    let start = Instant::now();
    for (id, asset) in assets.iter().enumerate() {
        if asset.cohort != "original" && original.is_none() {
            original = Some(
                serde_json::json!({"images":samples.len(),"objects":objects.len(),"object_bytes":object_bytes,"recipe_bytes":recipe_bytes,"container_bytes":object_bytes+recipe_bytes,"sprite_images":selected}),
            );
        }
        if asset.kind != "png"
            || (args.get(4).is_some_and(|s| s == "original") && asset.cohort != "original")
        {
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
        let hash = blake3::hash(&img.pixels).to_hex().to_string();
        let (whole, encoded) = encode(&img.pixels, img.width, img.height, img.channels)?;
        let mut recipe = token(&Recipe::Delta {
            frame: whole.clone(),
            base: None,
            dy: 0,
            subtract: false,
            hash: hash.clone(),
        })?;
        let independent_bytes = encoded.len();
        let mut cost = recipe.len()
            + if objects.contains(&whole.hash) {
                0
            } else {
                encoded.len()
            };
        let mut pending = HashMap::new();
        pending.insert(whole.hash.clone(), encoded);
        let mut additions = Vec::new();
        let mut sprite_count = 0;
        let mut use_sprites = false;
        match prepare(&img, residual) {
            Ok((mut background, shapes, base_pixels)) => {
                let mut packs = Vec::new();
                let mut local = HashMap::<Key, usize>::new();
                let mut placements = Vec::new();
                let mut blobs = HashMap::new();
                let mut new_keys = Vec::new();
                let base = if let Some(base_pixels) = base_pixels {
                    let (p, bytes) = encode(&base_pixels, img.width, img.height, img.channels)?;
                    if !objects.contains(&p.hash) {
                        blobs.insert(p.hash.clone(), bytes);
                    }
                    packs.push(p);
                    background.clear();
                    Some(0)
                } else {
                    None
                };
                for shape in shapes {
                    let key = (
                        shape.width,
                        shape.height,
                        shape.channels,
                        blake3::hash(&shape.pixels),
                    );
                    let index = if let Some(&index) = local.get(&key) {
                        index
                    } else {
                        let packed = if let Some(p) = known.get(&key) {
                            p.clone()
                        } else {
                            let (p, bytes) =
                                encode(&shape.pixels, shape.width, shape.height, shape.channels)?;
                            if !objects.contains(&p.hash) {
                                blobs.insert(p.hash.clone(), bytes);
                            }
                            new_keys.push((key, p.clone()));
                            p
                        };
                        let index = packs.len();
                        packs.push(packed);
                        local.insert(key, index);
                        index
                    };
                    placements.push([index as u32, shape.x as u32, shape.y as u32, shape.invert]);
                }
                sprite_count = placements.len();
                let candidate = token(&Recipe::Sprites {
                    width: img.width,
                    height: img.height,
                    channels: img.channels,
                    hash,
                    tile: TILE,
                    background,
                    packs,
                    placements,
                    base,
                })?;
                let candidate_cost = candidate.len() + blobs.values().map(Vec::len).sum::<usize>();
                if candidate_cost < cost {
                    cost = candidate_cost;
                    recipe = candidate;
                    pending = blobs;
                    additions = new_keys;
                    use_sprites = true;
                }
            }
            Err(_) => rejected += 1,
        }
        for (hash, bytes) in pending {
            if objects.insert(hash.clone()) {
                pack::atomic_write_immutable(&out.join("objects").join(&hash), &bytes)?;
                object_bytes += bytes.len() as u64;
            }
        }
        for (key, packed) in additions {
            known.insert(key, packed);
        }
        let path = out.join("recipes").join(format!("{id:06}.pixel"));
        fs::write(&path, &recipe)?;
        recipe_bytes += recipe.len() as u64;
        let verify = Instant::now();
        ensure!(
            reader.resolve(&fs::read(path)?)? == img.pixels,
            "sprite pixel reconstruction differs"
        );
        restore_seconds += verify.elapsed().as_secs_f64();
        if use_sprites {
            selected += 1;
        }
        samples.push(serde_json::json!({"ordinal":id,"cohort":asset.cohort,"pixels_bytes":img.pixels.len(),"independent_payload_bytes":independent_bytes,"marginal_stored_bytes":cost,"recipe_bytes":recipe.len(),"sprite_components":sprite_count,"selected_sprites":use_sprites}));
        if samples.len().is_multiple_of(25) {
            eprintln!(
                "{} exact images; {selected} sprite recipes; {} objects; {:.1}s",
                samples.len(),
                objects.len(),
                start.elapsed().as_secs_f64()
            );
        }
    }
    let report = serde_json::json!({"candidate":"exact-polarity-normalized-sprites","scope":"Complete pixel collection with exact background and residual shapes; PNG reconstruction excluded; one exploratory trial; leaf objects not yet packed","original":original,"images":samples.len(),"skipped_images":skipped,"segmentation_budget_fallbacks":rejected,"sprite_images":selected,"background_residual":residual,"objects":objects.len(),"object_bytes":object_bytes,"recipe_bytes":recipe_bytes,"container_bytes":object_bytes+recipe_bytes,"exact_pixel_restores":samples.len(),"samples":samples,"wall_seconds":start.elapsed().as_secs_f64(),"restore_seconds":restore_seconds});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!(
        "{}",
        serde_json::json!({"images":samples.len(),"sprite_images":selected,"bytes":object_bytes+recipe_bytes,"objects":objects.len()})
    );
    Ok(())
}
