#!/usr/bin/env python3
"""Fetch pinned public JSONL traces outside the checkout; print aggregates only."""

import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import tempfile
import time
import traceback
import urllib.parse
import urllib.request


DATASETS = {
    "agent-sessions-list": {
        "repository": "cfahlgren1/agent-sessions-list",
        "revision": "e40be427ffb9b8a3ef31a7cb22952e4722eaf1a0",
        "attribution": "cfahlgren1 and the agent-sessions-list contributors",
        "license_note": "No license is declared in the pinned dataset metadata; trace contents are not redistributed.",
    },
    "trace-commons": {
        "repository": "trace-commons/agent-traces",
        "revision": "112ebd4d03ce852b00e935d523107c3d0c9a65bf",
        "attribution": "Trace Commons and contributing session authors",
        "license_note": "Dataset compilation is CC-BY-4.0; individual trace contents may carry separate licenses. Trace contents are not redistributed.",
    },
}


def request(url):
    return urllib.request.urlopen(
        urllib.request.Request(url, headers={"User-Agent": "nenia-public-trace-validation/0.1"}),
        timeout=60,
    )


def json_request(url):
    with request(url) as response:
        data = response.read(16 * 1024 * 1024 + 1)
        if len(data) > 16 * 1024 * 1024:
            raise ValueError("dataset metadata exceeds limit")
        return json.loads(data), response.headers.get("Link", "")


def private_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as handle:
        temporary = Path(handle.name)
        json.dump(value, handle, indent=2, sort_keys=True)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    temporary.replace(path)


def entries_for(source):
    base = f"https://huggingface.co/api/datasets/{source['repository']}"
    info, _ = json_request(f"{base}/revision/{source['revision']}")
    if info.get("sha") != source["revision"]:
        raise ValueError("dataset revision did not match the pinned commit")
    url = f"{base}/tree/{source['revision']}?recursive=true&expand=false&limit=1000"
    entries = []
    visited = set()
    while url:
        if url in visited:
            raise ValueError("dataset listing pagination repeated")
        visited.add(url)
        page, links = json_request(url)
        entries.extend(entry for entry in page if entry.get("type") == "file" and entry["path"].lower().endswith(".jsonl"))
        url = None
        for link in links.split(","):
            if 'rel="next"' in link:
                candidate = link.split(";", 1)[0].strip().strip("<>")
                if not candidate.startswith(base + "/tree/" + source["revision"]):
                    raise ValueError("unexpected dataset pagination URL")
                url = candidate
    if not entries:
        raise ValueError("pinned dataset has no raw JSONL files")
    return info, sorted(entries, key=lambda entry: entry["path"])


def provider_for(relative):
    parts = PurePosixPath(relative).parts
    provider = parts[1] if parts[0] == "sessions" and len(parts) > 1 else parts[0]
    return {"claude_code": "claude"}.get(provider, provider)


def digest_file(path, size):
    sha256 = hashlib.sha256()
    git_blob = hashlib.sha1(f"blob {size}\0".encode())
    count = 0
    with path.open("rb") as handle:
        while data := handle.read(1024 * 1024):
            sha256.update(data)
            git_blob.update(data)
            count += len(data)
    return count, sha256.hexdigest(), git_blob.hexdigest()


def verified_manifest_entry(path, entry):
    count, sha256, git_blob = digest_file(path, entry["size"])
    expected = entry.get("lfs", {}).get("oid") or entry["oid"]
    actual = sha256 if entry.get("lfs") else git_blob
    if count != entry["size"] or actual != expected:
        raise ValueError("download does not match pinned upstream size and object hash")
    return {"path": entry["path"], "bytes": count, "sha256": sha256, "provider": provider_for(entry["path"])}


def fetch_entry(source, raw, entry):
    relative = PurePosixPath(entry["path"])
    if relative.is_absolute() or any(part in (".", "..") for part in relative.parts) or "\\" in entry["path"]:
        raise ValueError("unsafe dataset file path")
    path = raw.joinpath(*relative.parts)
    if not path.resolve().is_relative_to(raw.resolve()):
        raise ValueError("dataset path escapes private output")
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    if path.is_file():
        return verified_manifest_entry(path, entry)
    url = f"https://huggingface.co/datasets/{source['repository']}/resolve/{source['revision']}/{urllib.parse.quote(entry['path'], safe='/')}?download=true"
    for attempt in range(4):
        temporary = None
        try:
            with request(url) as response, tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as handle:
                temporary = Path(handle.name)
                count = 0
                while data := response.read(1024 * 1024):
                    count += len(data)
                    if count > entry["size"]:
                        raise ValueError("download exceeded pinned object size")
                    handle.write(data)
                handle.flush()
                os.fsync(handle.fileno())
            manifest = verified_manifest_entry(temporary, entry)
            temporary.replace(path)
            return manifest
        except Exception:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
            if attempt == 3:
                raise
            time.sleep(2 ** attempt)


def fetch_dataset(name, output):
    source = DATASETS[name]
    directory = output / name / source["revision"]
    raw = directory / "raw"
    raw.mkdir(parents=True, exist_ok=True, mode=0o700)
    info, entries = entries_for(source)
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        manifests = list(executor.map(lambda entry: fetch_entry(source, raw, entry), entries))
    providers = {}
    for entry in manifests:
        counts = providers.setdefault(entry["provider"], {"files": 0, "bytes": 0})
        counts["files"] += 1
        counts["bytes"] += entry["bytes"]
    summary = {
        "dataset": source["repository"],
        "url": f"https://huggingface.co/datasets/{source['repository']}",
        "revision": source["revision"],
        "attribution": source["attribution"],
        "declared_license": info.get("cardData", {}).get("license"),
        "license_note": source["license_note"],
        "files": len(manifests),
        "bytes": sum(entry["bytes"] for entry in manifests),
        "providers": providers,
        "selection": "All raw .jsonl files at the pinned revision; no parquet, metadata, or README files.",
        "integrity": "Each file matched upstream byte length and Git blob SHA-1 or LFS SHA-256.",
        "redistributed_trace_contents": False,
    }
    private_json(directory / "download-manifest.json", {"source": source, "files": manifests})
    private_json(directory / "source-summary.json", summary)
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path, help="Private cache directory outside this repository")
    parser.add_argument("--dataset", choices=["all", *DATASETS], default="all")
    args = parser.parse_args()
    output = args.output.expanduser().resolve()
    repository = Path(__file__).resolve().parents[1]
    if output.is_relative_to(repository) or repository.is_relative_to(output):
        parser.error("choose a private cache outside, and not containing, this checkout")
    output.mkdir(parents=True, exist_ok=True, mode=0o700)
    output.chmod(0o700)
    selected = DATASETS if args.dataset == "all" else [args.dataset]
    try:
        for name in selected:
            print(json.dumps(fetch_dataset(name, output), sort_keys=True), flush=True)
    except Exception:
        # Full errors may contain filenames or signed download URLs; keep them private.
        with (output / "download-errors.log").open("a", encoding="utf-8") as handle:
            os.chmod(handle.name, 0o600)
            traceback.print_exc(file=handle)
        raise SystemExit("Download failed; diagnostic details are in the private cache download-errors.log.")


if __name__ == "__main__":
    main()
