//! Joint representation planning over immutable shared pixel objects. Minimize
//! actual stored bytes, including dependencies and per-image recipes. Coordinate
//! moves plus shared-object opening/closing moves escape simple greedy choices.
use anyhow::{Result, ensure};
use nenia_media_screen::{
    png,
    shared_pixels::{Prepared, Reader},
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
    time::Instant,
};
#[derive(Deserialize)]
struct Asset {
    file: String,
    kind: String,
    cohort: String,
}
struct Choice {
    source: usize,
    recipe: Vec<u8>,
    objects: Vec<usize>,
}
struct Image {
    ordinal: usize,
    choices: Vec<Choice>,
    selected: usize,
}
fn total(images: &[Image], counts: &[u32], costs: &[u64]) -> u64 {
    images
        .iter()
        .map(|i| i.choices[i.selected].recipe.len() as u64)
        .sum::<u64>()
        + counts
            .iter()
            .zip(costs)
            .filter(|(n, _)| **n > 0)
            .map(|(_, c)| *c)
            .sum::<u64>()
}
fn delta(images: &[Image], changes: &[(usize, usize)], counts: &[u32], costs: &[u64]) -> i64 {
    let mut refs = HashMap::<usize, i32>::new();
    let mut cost = 0i64;
    for &(i, new) in changes {
        let image = &images[i];
        let old = &image.choices[image.selected];
        let new = &image.choices[new];
        cost += new.recipe.len() as i64 - old.recipe.len() as i64;
        for &id in &old.objects {
            *refs.entry(id).or_default() -= 1;
        }
        for &id in &new.objects {
            *refs.entry(id).or_default() += 1;
        }
    }
    for (id, n) in refs {
        let before = counts[id] as i64;
        let after = before + n as i64;
        assert!(after >= 0);
        if before == 0 && after > 0 {
            cost += costs[id] as i64;
        }
        if before > 0 && after == 0 {
            cost -= costs[id] as i64;
        }
    }
    cost
}
fn apply(images: &mut [Image], changes: &[(usize, usize)], counts: &mut [u32]) {
    for &(i, new) in changes {
        let image = &mut images[i];
        for &id in &image.choices[image.selected].objects {
            counts[id] -= 1;
        }
        for &id in &image.choices[new].objects {
            counts[id] += 1;
        }
        image.selected = new;
    }
}
fn coordinates(images: &mut [Image], counts: &mut [u32], costs: &[u64]) -> usize {
    let mut moves = 0;
    for _ in 0..32 {
        let before = moves;
        for i in 0..images.len() {
            let (change, candidate) = (0..images[i].choices.len())
                .map(|c| (delta(images, &[(i, c)], counts, costs), c))
                .min()
                .unwrap();
            if change < 0 {
                apply(images, &[(i, candidate)], counts);
                moves += 1;
            }
        }
        if moves == before {
            break;
        }
    }
    moves
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() >= 6,
        "usage: pixel-plan PRIVATE_ASSETS PRIVATE_OUTPUT REPORT original|all PRIVATE_CANDIDATES..."
    );
    let cohort = args[4].strip_suffix("-canonical").unwrap_or(&args[4]);
    ensure!(cohort == "original" || cohort == "all", "cohort");
    let input = Path::new(&args[1]);
    let output = Path::new(&args[2]);
    ensure!(!output.exists(), "output exists");
    fs::create_dir_all(output.join("objects"))?;
    fs::create_dir(output.join("recipes"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(output, fs::Permissions::from_mode(0o700))?;
    }
    let assets: Vec<Asset> = serde_json::from_slice(&fs::read(input.join("assets.json"))?)?;
    let mut sources = args[5..]
        .iter()
        .map(|p| Prepared::open(Path::new(p)))
        .collect::<Result<Vec<_>>>()?;
    let canonical_start = Instant::now();
    let canonical = if args[4].ends_with("-canonical") {
        Some(Prepared::canonicalize(&mut sources)?)
    } else {
        None
    };
    let canonical_seconds = canonical_start.elapsed().as_secs_f64();
    let mut objects = HashMap::<String, usize>::new();
    let mut costs = Vec::new();
    let mut images = Vec::new();
    let start = Instant::now();
    for (ordinal, asset) in assets.iter().enumerate() {
        if asset.kind != "png" || (cohort == "original" && asset.cohort != "original") {
            continue;
        }
        let mut choices = Vec::new();
        for (source, prepared) in sources.iter().enumerate() {
            if let Some((recipe, leaves)) = prepared.quote(ordinal)? {
                let mut ids = Vec::new();
                for (hash, cost) in leaves {
                    let id = *objects.entry(hash).or_insert_with(|| {
                        costs.push(cost);
                        costs.len() - 1
                    });
                    ensure!(costs[id] == cost, "shared object size mismatch");
                    ids.push(id);
                }
                ids.sort_unstable();
                ids.dedup();
                choices.push(Choice {
                    source,
                    recipe,
                    objects: ids,
                });
            }
        }
        if !choices.is_empty() {
            images.push(Image {
                ordinal,
                choices,
                selected: 0,
            });
        }
    }
    let mut counts = vec![0; costs.len()];
    for image in &mut images {
        let (_, selected) = image
            .choices
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    c.recipe.len() as u64
                        + c.objects
                            .iter()
                            .filter(|&&id| counts[id] == 0)
                            .map(|&id| costs[id])
                            .sum::<u64>(),
                    i,
                )
            })
            .min()
            .unwrap();
        image.selected = selected;
        for &id in &image.choices[selected].objects {
            counts[id] += 1;
        }
    }
    let greedy = total(&images, &counts, &costs);
    let mut coordinate_moves = coordinates(&mut images, &mut counts, &costs);
    let coordinate = total(&images, &counts, &costs);
    let mut owners = vec![Vec::new(); costs.len()];
    for (i, image) in images.iter().enumerate() {
        for (c, choice) in image.choices.iter().enumerate() {
            for &id in &choice.objects {
                owners[id].push((i, c));
            }
        }
    }
    let mut opening_moves = 0;
    let mut closing_moves = 0;
    for _ in 0..4 {
        let before = total(&images, &counts, &costs);
        for object in 0..costs.len() {
            if counts[object] != 0 || owners[object].len() < 2 {
                continue;
            }
            let mut best = BTreeMap::<usize, (i64, usize)>::new();
            for &(i, c) in &owners[object] {
                let change = delta(&images, &[(i, c)], &counts, &costs) - costs[object] as i64;
                if change < 0 {
                    let entry = best.entry(i).or_insert((change, c));
                    if (change, c) < *entry {
                        *entry = (change, c);
                    }
                }
            }
            let changes = best
                .into_iter()
                .map(|(i, (_, c))| (i, c))
                .collect::<Vec<_>>();
            if changes.len() >= 2 && delta(&images, &changes, &counts, &costs) < 0 {
                apply(&mut images, &changes, &mut counts);
                opening_moves += 1;
            }
        }
        for object in 0..costs.len() {
            if counts[object] < 2 || costs[object] < 4096 {
                continue;
            }
            let mut users = owners[object]
                .iter()
                .filter(|&&(i, c)| images[i].selected == c)
                .map(|&(i, _)| i)
                .collect::<Vec<_>>();
            users.sort_unstable();
            users.dedup();
            let mut changes = Vec::new();
            for i in users {
                let candidate = images[i]
                    .choices
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.objects.binary_search(&object).is_err())
                    .map(|(c, _)| (delta(&images, &[(i, c)], &counts, &costs), c))
                    .min();
                if let Some((_, c)) = candidate {
                    changes.push((i, c));
                } else {
                    changes.clear();
                    break;
                }
            }
            if !changes.is_empty() && delta(&images, &changes, &counts, &costs) < 0 {
                apply(&mut images, &changes, &mut counts);
                closing_moves += 1;
            }
        }
        coordinate_moves += coordinates(&mut images, &mut counts, &costs);
        if total(&images, &counts, &costs) == before {
            break;
        }
    }
    let planned = total(&images, &counts, &costs);
    let planning_seconds = start.elapsed().as_secs_f64();
    let mut selection = vec![0; sources.len()];
    for image in &images {
        let choice = &image.choices[image.selected];
        let token = sources[choice.source]
            .materialize(image.ordinal, &output.join("objects"))?
            .unwrap();
        ensure!(token == choice.recipe, "pixel candidate changed");
        fs::write(
            output
                .join("recipes")
                .join(format!("{:06}.pixel", image.ordinal)),
            token,
        )?;
        selection[choice.source] += 1;
    }
    let start = Instant::now();
    let mut reader = Reader::new(output.join("objects"));
    for image in &images {
        let token = fs::read(
            output
                .join("recipes")
                .join(format!("{:06}.pixel", image.ordinal)),
        )?;
        let pixels = reader.resolve(&token)?;
        let original = png::parse(&fs::read(input.join(&assets[image.ordinal].file))?)?;
        ensure!(
            pixels == original.pixels,
            "planned pixel reconstruction differs"
        );
    }
    let bytes = fs::read_dir(output.join("objects"))?
        .chain(fs::read_dir(output.join("recipes"))?)
        .map(|p| p.and_then(|p| p.metadata()).map(|m| m.len()))
        .collect::<std::io::Result<Vec<_>>>()?
        .iter()
        .sum::<u64>();
    ensure!(
        bytes == planned,
        "planner byte accounting differs from disk"
    );
    let report = serde_json::json!({"candidate":"joint-pixel-representation-planner","scope":"Complete pixel-only archive; exact marginal object and recipe costs; PNG reconstruction excluded; one exploratory trial","cohort":cohort,"canonicalization":canonical,"canonicalization_seconds":canonical_seconds,"images":images.len(),"candidate_sources":sources.len(),"candidate_objects":costs.len(),"selected_objects":counts.iter().filter(|&&n|n>0).count(),"selection_by_source_order":selection,"greedy_bytes":greedy,"coordinate_bytes":coordinate,"container_bytes":bytes,"coordinate_moves":coordinate_moves,"shared_object_openings":opening_moves,"shared_object_closings":closing_moves,"planning_seconds":planning_seconds,"restore_with_oracle_seconds":start.elapsed().as_secs_f64(),"exact_pixel_restores":images.len(),"optimality":"local search; no claim of a global optimum"});
    fs::write(&args[3], serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    Ok(())
}
