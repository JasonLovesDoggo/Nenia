#!/usr/bin/env python3
"""Extract private, ordinal-named real PNG/JPEG assets; publish aggregates only.

Scans canonical base64 embedded anywhere in JSONL, including nested JSON strings.
This is a research inventory, not the production parser or a backup format.
"""
import argparse
import base64
import collections
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--corpus', type=Path, required=True)
p.add_argument('--private-root', type=Path, required=True)
p.add_argument('--report', type=Path, required=True)
a = p.parse_args()
repo = Path(__file__).resolve().parents[1]
if a.private_root.resolve().is_relative_to(repo):
    p.error('private output must be outside repository')
a.private_root.mkdir(mode=0o700, parents=True, exist_ok=False)
files = json.loads(a.corpus.read_text())['files']
files.sort(key=lambda f: (f['provider'].startswith('additional_'), f['provider'], f['copy']))
pattern = re.compile(rb'(?:iVBORw0KGgo|/9j/)[A-Za-z0-9+/]*={0,2}')
seen = {}
assets = []
counts = collections.Counter()
original = None
started = time.monotonic()
for i, entry in enumerate(files):
    source = Path(entry['copy'])
    proc = None
    if source.suffix == '.zst':
        proc = subprocess.Popen(['zstd', '-qdc', str(source)], stdout=subprocess.PIPE)
        inp = proc.stdout
    else:
        inp = source.open('rb')
    cohort = 'additional' if entry['provider'].startswith('additional_') else 'original'
    if cohort == 'additional' and original is None:
        original = dict(counts)
    for line in inp:
        for match in pattern.finditer(line):
            encoded = match.group()
            try:
                data = base64.b64decode(encoded, validate=True)
            except ValueError:
                continue
            if base64.b64encode(data) != encoded:
                continue
            kind = 'png' if data.startswith(b'\x89PNG\r\n\x1a\n') else 'jpeg'
            key = hashlib.sha256(data).hexdigest()
            counts[kind + '_occurrences'] += 1
            counts[kind + '_bytes'] += len(data)
            if key in seen:
                assets[seen[key]]['occurrences'] += 1
                continue
            index = len(assets)
            name = f'{index:06}.{kind}'
            (a.private_root / name).write_bytes(data)
            seen[key] = index
            assets.append({'file': name, 'bytes': len(data), 'sha256': key,
                           'cohort': cohort, 'kind': kind, 'occurrences': 1})
            counts[kind + '_unique'] += 1
            counts[kind + '_unique_bytes'] += len(data)
    inp.close()
    if proc and proc.wait() != 0:
        raise RuntimeError('source decode failed')
    if (i + 1) % 100 == 0:
        print(json.dumps({'files': i + 1, 'unique_assets': len(assets),
                          'seconds': round(time.monotonic() - started, 2)}), flush=True)
(a.private_root / 'assets.json').write_text(json.dumps(assets) + '\n')
report = {'inventory': 'Canonical base64 PNG/JPEG signatures; not an archive estimate',
          'files': len(files), 'original': original or dict(counts), 'complete': dict(counts),
          'seconds': time.monotonic() - started, 'private_content_published': False}
a.report.parent.mkdir(parents=True, exist_ok=True)
a.report.write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report))
