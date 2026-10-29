use crate::deflate_recipe;
use anyhow::{Result, ensure};
use std::{collections::BTreeMap, io::Read};
const LIMIT: usize = 128 * 1024 * 1024;
const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
fn number(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&(n as u32).to_le_bytes());
}
fn blob(out: &mut Vec<u8>, data: &[u8]) {
    number(out, data.len());
    out.extend_from_slice(data);
}
fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    ensure!(n <= input.len(), "truncated PNG recipe");
    let out = &input[..n];
    *input = &input[n..];
    Ok(out)
}
fn integer(input: &mut &[u8]) -> Result<usize> {
    Ok(u32::from_le_bytes(take(input, 4)?.try_into()?) as usize)
}
fn section<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let n = integer(input)?;
    ensure!(n <= LIMIT, "section limit");
    take(input, n)
}
fn zip(data: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::stream::encode_all(data, 9)?)
}
fn unzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = zstd::stream::read::Decoder::new(data)?;
    decoder.window_log_max(27)?;
    let mut out = Vec::new();
    decoder.take(LIMIT as u64 + 1).read_to_end(&mut out)?;
    ensure!(out.len() <= LIMIT, "decoded section limit");
    Ok(out)
}
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub channels: usize,
    pub depth: usize,
    pub pixels: Vec<u8>,
    pub filters: Vec<u8>,
    pub shell: Vec<u8>,
    pub idat: Vec<u8>,
}
pub fn parse(data: &[u8]) -> Result<Image> {
    ensure!(
        data.len() <= LIMIT && data.starts_with(SIGNATURE),
        "PNG signature/size"
    );
    let mut input = &data[8..];
    let mut shell = SIGNATURE.to_vec();
    let mut idat = Vec::new();
    let mut ihdr = Vec::new();
    while !input.is_empty() {
        let head = take(&mut input, 8)?;
        let n = u32::from_be_bytes(head[..4].try_into()?) as usize;
        let content = take(&mut input, n)?;
        let crc = take(&mut input, 4)?;
        shell.extend_from_slice(head);
        if &head[4..] == b"IDAT" {
            idat.extend_from_slice(content);
        } else {
            shell.extend_from_slice(content);
        }
        shell.extend_from_slice(crc);
        if &head[4..] == b"IHDR" {
            ensure!(ihdr.is_empty(), "duplicate header");
            ihdr = content.to_vec();
        }
        if &head[4..] == b"IEND" {
            ensure!(input.is_empty(), "PNG trailing bytes");
            break;
        }
    }
    ensure!(
        ihdr.len() == 13 && ihdr[10..] == [0, 0, 0],
        "unsupported PNG layout"
    );
    let width = u32::from_be_bytes(ihdr[..4].try_into()?) as usize;
    let height = u32::from_be_bytes(ihdr[4..8].try_into()?) as usize;
    let depth = ihdr[8] as usize;
    let channels = match ihdr[9] {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        6 => 4,
        _ => 0,
    };
    ensure!(
        width > 0
            && height > 0
            && width <= 16384
            && height <= 16384
            && channels > 0
            && matches!(depth, 1 | 2 | 4 | 8 | 16),
        "PNG dimensions"
    );
    let row = (width * depth * channels).div_ceil(8);
    let bpp = (depth * channels).div_ceil(8);
    ensure!((row + 1) * height <= LIMIT, "PNG pixel budget");
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(idat.as_slice())
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut raw)?;
    ensure!(raw.len() == (row + 1) * height, "PNG scanline size");
    let filters = raw.chunks_exact(row + 1).map(|r| r[0]).collect::<Vec<_>>();
    let mut pixels = vec![0; row * height];
    for y in 0..height {
        for x in 0..row {
            let pred = predict(&pixels, y, row, x, bpp, filters[y])?;
            pixels[y * row + x] = raw[y * (row + 1) + 1 + x].wrapping_add(pred);
        }
    }
    Ok(Image {
        width,
        height,
        channels,
        depth,
        pixels,
        filters,
        shell,
        idat,
    })
}
fn predict(pixels: &[u8], y: usize, row: usize, x: usize, bpp: usize, filter: u8) -> Result<u8> {
    let a = if x >= bpp {
        pixels[y * row + x - bpp]
    } else {
        0
    };
    let b = if y > 0 { pixels[(y - 1) * row + x] } else { 0 };
    let c = if x >= bpp && y > 0 {
        pixels[(y - 1) * row + x - bpp]
    } else {
        0
    };
    Ok(match filter {
        0 => 0,
        1 => a,
        2 => b,
        3 => ((a as u16 + b as u16) / 2) as u8,
        4 => {
            let p = a as i32 + b as i32 - c as i32;
            let pa = (p - a as i32).abs();
            let pb = (p - b as i32).abs();
            let pc = (p - c as i32).abs();
            if pa <= pb && pa <= pc {
                a
            } else if pb <= pc {
                b
            } else {
                c
            }
        }
        _ => anyhow::bail!("invalid PNG filter"),
    })
}
pub fn refilter(pixels: &[u8], filters: &[u8], row: usize, bpp: usize) -> Result<Vec<u8>> {
    ensure!(
        row > 0
            && bpp > 0
            && pixels.len() == row * filters.len()
            && pixels.len() + filters.len() <= LIMIT,
        "filter layout"
    );
    let mut raw = Vec::with_capacity(pixels.len() + filters.len());
    for (y, &filter) in filters.iter().enumerate() {
        raw.push(filter);
        for x in 0..row {
            raw.push(pixels[y * row + x].wrapping_sub(predict(pixels, y, row, x, bpp, filter)?));
        }
    }
    Ok(raw)
}
pub fn webp_encode(pixels: &[u8], width: usize, height: usize, channels: usize) -> Result<Vec<u8>> {
    ensure!(matches!(channels, 3 | 4), "unsupported WebP channels");
    let layout = if channels == 4 {
        webp::PixelLayout::Rgba
    } else {
        webp::PixelLayout::Rgb
    };
    let encoder = webp::Encoder::new(pixels, layout, width as u32, height as u32);
    let mut config = webp::WebPConfig::new().unwrap();
    config.lossless = 1;
    config.exact = 1;
    config.quality = 75.;
    config.method = 4;
    encoder
        .encode_advanced(&config)
        .map(|v| v.to_vec())
        .map_err(|e| anyhow::anyhow!("WebP encode {e:?}"))
}
pub fn webp_decode(data: &[u8], width: usize, height: usize, channels: usize) -> Result<Vec<u8>> {
    ensure!(
        matches!(channels, 3 | 4) && width * height * channels <= LIMIT,
        "WebP output budget"
    );
    let features =
        webp::BitstreamFeatures::new(data).ok_or_else(|| anyhow::anyhow!("invalid WebP"))?;
    ensure!(
        features.width() as usize == width && features.height() as usize == height,
        "WebP dimensions mismatch"
    );
    let decoded = webp::Decoder::new(data)
        .decode()
        .ok_or_else(|| anyhow::anyhow!("WebP decode failed"))?;
    if channels == 4 && !decoded.is_alpha() {
        Ok(decoded
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect())
    } else {
        ensure!(
            decoded.len() == width * height * channels,
            "WebP channel mismatch"
        );
        Ok(decoded.to_vec())
    }
}
pub fn jxl_decode(data: &[u8], width: usize, height: usize, channels: usize) -> Result<Vec<u8>> {
    ensure!(
        width > 0
            && height > 0
            && width <= 16384
            && height <= 16384
            && (1..=4).contains(&channels)
            && width * height * channels <= LIMIT,
        "JXL pixel budget"
    );
    let image = jxl_oxide::JxlImage::builder()
        .alloc_tracker(jxl_oxide::AllocTracker::with_limit(512 * 1024 * 1024))
        .read(data)
        .map_err(|e| anyhow::anyhow!("JXL header: {e}"))?;
    ensure!(
        image.width() as usize == width
            && image.height() as usize == height
            && image.pixel_format().channels() == channels
            && image.num_loaded_keyframes() == 1,
        "JXL layout"
    );
    let render = image
        .render_frame(0)
        .map_err(|e| anyhow::anyhow!("JXL decode: {e}"))?;
    let mut pixels = vec![0u8; width * height * channels];
    let n = render.stream().write_to_buffer(&mut pixels);
    ensure!(n == pixels.len(), "JXL output size");
    Ok(pixels)
}
fn frame(data: &[u8], mode: u8, body: &[u8]) -> Vec<u8> {
    let mut out = b"NENIAPG1".to_vec();
    out.push(mode);
    number(&mut out, data.len());
    out.extend_from_slice(blake3::hash(data).as_bytes());
    out.extend_from_slice(body);
    out
}
pub fn encode(data: &[u8]) -> Result<(Vec<u8>, BTreeMap<String, u64>)> {
    let mut stats = BTreeMap::new();
    let fallback = frame(data, 0, &zip(data)?);
    let img = match parse(data) {
        Ok(v) => v,
        Err(_) => {
            stats.insert("unsupported_png".into(), 1);
            return Ok((fallback, stats));
        }
    };
    ensure!(img.idat.len() >= 6, "short IDAT");
    let encoded = deflate_recipe::encode(&img.idat[2..img.idat.len() - 4])?;
    let raw = refilter(
        &img.pixels,
        &img.filters,
        (img.width * img.depth * img.channels).div_ceil(8),
        (img.depth * img.channels).div_ceil(8),
    )?;
    ensure!(raw == encoded.plain, "scanline inverse differs");
    ensure!(
        deflate_recipe::decode(&raw, &encoded.recipe)? == img.idat[2..img.idat.len() - 4],
        "custom DEFLATE reconstruction failed"
    );
    let mut custom = zip(&encoded.recipe)?;
    let ranked = deflate_recipe::rank_recipe(&raw, &encoded.recipe)?;
    ensure!(
        deflate_recipe::decode(&raw, &ranked)? == img.idat[2..img.idat.len() - 4],
        "ranked DEFLATE differs"
    );
    let ranked = zip(&ranked)?;
    stats.insert("physical_distance_recipe_bytes".into(), custom.len() as u64);
    stats.insert("witness_rank_recipe_bytes".into(), ranked.len() as u64);
    if ranked.len() < custom.len() {
        custom = ranked;
        stats.insert("witness_rank_selected".into(), 1);
    }
    stats.insert(
        "alternate_258_matches".into(),
        encoded.alternate_lengths as u64,
    );
    stats.insert("custom_recipe_bytes".into(), custom.len() as u64);
    let mut correction = custom;
    let mut correction_kind = 0;
    match preflate_rs::preflate_whole_deflate_stream(
        &img.idat[2..img.idat.len() - 4],
        &preflate_rs::PreflateConfig {
            plain_text_limit: LIMIT,
            verify_compression: true,
            ..Default::default()
        },
    ) {
        Ok((v, text)) => {
            ensure!(
                text.text() == raw && v.compressed_size == img.idat.len() - 6,
                "Preflate plaintext/size"
            );
            ensure!(
                preflate_rs::recreate_whole_deflate_stream(&raw, &v.corrections)?
                    == img.idat[2..img.idat.len() - 4],
                "Preflate reconstruction differs"
            );
            stats.insert("preflate_successes".into(), 1);
            stats.insert("preflate_recipe_bytes".into(), v.corrections.len() as u64);
            let candidate = zip(&v.corrections)?;
            if candidate.len() < correction.len() {
                correction = candidate;
                correction_kind = 1;
            }
        }
        Err(e) => {
            stats.insert(format!("preflate_error_{:?}", e.exit_code()), 1);
        }
    }
    // A historical encoder is optional: accept only a complete byte proof.
    // The generic recipe remains available for every other compressor.
    for row in [0, (img.width * img.depth * img.channels).div_ceil(8) + 1] {
        if fdeflate(&raw, row)? == img.idat {
            correction = zip(&[])?;
            correction_kind = if row == 0 { 2 } else { 3 };
            stats.insert(format!("exact_fdeflate_profile_{correction_kind}"), 1);
            break;
        }
    }
    let mut pixel = zip(&img.pixels)?;
    let mut pixel_kind = 0;
    if img.depth == 8 && matches!(img.channels, 3 | 4) {
        let candidate = webp_encode(&img.pixels, img.width, img.height, img.channels)?;
        ensure!(
            webp_decode(&candidate, img.width, img.height, img.channels)? == img.pixels,
            "WebP pixels differ"
        );
        if candidate.len() < pixel.len() {
            pixel = candidate;
            pixel_kind = 1;
        }
    }
    let mut side = Vec::new();
    for v in [img.width, img.height, img.channels, img.depth] {
        number(&mut side, v);
    }
    side.extend_from_slice(&img.idat[..2]);
    side.extend_from_slice(&img.idat[img.idat.len() - 4..]);
    blob(&mut side, &img.filters);
    blob(&mut side, &img.shell);
    let side = zip(&side)?;
    let mut body = vec![correction_kind, pixel_kind];
    blob(&mut body, &side);
    blob(&mut body, &correction);
    blob(&mut body, &pixel);
    let candidate = frame(data, 1, &body);
    stats.insert("independent_pixel_bytes".into(), pixel.len() as u64);
    stats.insert("selected_correction_bytes".into(), correction.len() as u64);
    stats.insert("side_bytes".into(), side.len() as u64);
    stats.insert("pixels_bytes".into(), img.pixels.len() as u64);
    if candidate.len() < fallback.len() {
        stats.insert(format!("selected_correction_{correction_kind}"), 1);
        stats.insert(format!("selected_pixels_{pixel_kind}"), 1);
        Ok((candidate, stats))
    } else {
        stats.insert("selected_opaque".into(), 1);
        Ok((fallback, stats))
    }
}
pub fn decode(input: &[u8]) -> Result<Vec<u8>> {
    decode_with_pixels(input, |_| {
        anyhow::bail!("external pixels require a resolver")
    })
}

pub fn externalize(input: &[u8], reference: &[u8]) -> Result<Option<Vec<u8>>> {
    ensure!(
        input.len() >= 45 && &input[..8] == b"NENIAPG1",
        "PNG container header"
    );
    if input[8] == 0 {
        return Ok(None);
    }
    ensure!(input[8] == 1, "PNG container mode");
    let mut body = &input[45..];
    let correction_kind = take(&mut body, 1)?[0];
    let _ = take(&mut body, 1)?;
    let side = section(&mut body)?;
    let correction = section(&mut body)?;
    let _ = section(&mut body)?;
    ensure!(body.is_empty(), "PNG body tail");
    let mut out = input[..45].to_vec();
    out.extend([correction_kind, 2]);
    blob(&mut out, side);
    blob(&mut out, correction);
    blob(&mut out, reference);
    Ok(Some(out))
}

pub fn decode_with_pixels(
    mut input: &[u8],
    mut resolve: impl FnMut(&[u8]) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    ensure!(
        input.len() <= LIMIT && take(&mut input, 8)? == b"NENIAPG1",
        "asset header/size"
    );
    let mode = take(&mut input, 1)?[0];
    let size = integer(&mut input)?;
    let hash = take(&mut input, 32)?;
    ensure!(size <= LIMIT, "asset output limit");
    let data = if mode == 0 {
        unzip(input)?
    } else {
        ensure!(mode == 1, "unknown asset mode");
        let correction_kind = take(&mut input, 1)?[0];
        let pixel_kind = take(&mut input, 1)?[0];
        let side = unzip(section(&mut input)?)?;
        let correction = unzip(section(&mut input)?)?;
        let pixel = section(&mut input)?;
        ensure!(input.is_empty(), "trailing asset data");
        let mut side = side.as_slice();
        let width = integer(&mut side)?;
        let height = integer(&mut side)?;
        let channels = integer(&mut side)?;
        let depth = integer(&mut side)?;
        ensure!(
            width > 0
                && height > 0
                && width <= 16384
                && height <= 16384
                && (1..=4).contains(&channels)
                && matches!(depth, 1 | 2 | 4 | 8 | 16),
            "pixel layout"
        );
        let prefix = take(&mut side, 2)?;
        let suffix = take(&mut side, 4)?;
        let filters = section(&mut side)?;
        let mut shell = section(&mut side)?;
        ensure!(side.is_empty() && filters.len() == height, "side layout");
        let pixels = match pixel_kind {
            0 => unzip(pixel)?,
            1 => {
                ensure!(depth == 8, "WebP bit depth");
                webp_decode(pixel, width, height, channels)?
            }
            2 => resolve(pixel)?,
            _ => anyhow::bail!("pixel codec"),
        };
        let raw = refilter(
            &pixels,
            filters,
            (width * channels * depth).div_ceil(8),
            (channels * depth).div_ceil(8),
        )?;
        let compressed = match correction_kind {
            0 => deflate_recipe::decode(&raw, &correction)?,
            1 => preflate_rs::recreate_whole_deflate_stream(&raw, &correction)?,
            2 | 3 => {
                ensure!(correction.is_empty(), "unexpected native correction");
                let row = if correction_kind == 2 {
                    0
                } else {
                    (width * channels * depth).div_ceil(8) + 1
                };
                let zlib = fdeflate(&raw, row)?;
                zlib[2..zlib.len() - 4].to_vec()
            }
            _ => anyhow::bail!("correction codec"),
        };
        let mut idat = prefix.to_vec();
        idat.extend(compressed);
        idat.extend(suffix);
        let mut idat = idat.as_slice();
        ensure!(take(&mut shell, 8)? == SIGNATURE, "shell signature");
        let mut out = SIGNATURE.to_vec();
        while !shell.is_empty() {
            let head = take(&mut shell, 8)?;
            let n = u32::from_be_bytes(head[..4].try_into()?) as usize;
            ensure!(
                out.len() + n + 12 <= size,
                "PNG shell exceeds original size"
            );
            out.extend(head);
            out.extend(if &head[4..] == b"IDAT" {
                take(&mut idat, n)?
            } else {
                take(&mut shell, n)?
            });
            out.extend(take(&mut shell, 4)?);
        }
        ensure!(idat.is_empty(), "unused IDAT bytes");
        out
    };
    ensure!(
        data.len() == size && blake3::hash(&data).as_bytes() == hash,
        "asset identity mismatch"
    );
    Ok(data)
}

fn fdeflate(raw: &[u8], row: usize) -> Result<Vec<u8>> {
    let mut encoder = fdeflate::Compressor::new(Vec::new())?;
    for bytes in raw.chunks(if row == 0 { raw.len().max(1) } else { row }) {
        encoder.write_data(bytes)?;
    }
    Ok(encoder.finish()?)
}
