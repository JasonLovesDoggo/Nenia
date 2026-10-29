#!/usr/bin/env python3
"""Aggregate lexical spelling diagnostic on actual frozen JSONL, no payload output."""
import argparse
import collections
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('corpus', type=Path)
p.add_argument('report', type=Path)
p.add_argument('--original', action='store_true')
a = p.parse_args()
files = json.loads(a.corpus.read_text())['files']
if a.original:
    files = [x for x in files if not x['provider'].startswith('additional_')]
pattern = re.compile(rb'"(?:[^"\\]|\\.)*"')
stats = collections.Counter()
seen = set()
start = time.monotonic()

def scan(data, depth=0):
    if depth == 3:
        return
    for match in pattern.finditer(data):
        token = match.group()
        if len(token) < 256:
            continue
        try:
            value = json.loads(token)
            raw = value.encode('utf-8')
        except (ValueError, UnicodeError):
            continue
        canonical = json.dumps(value, ensure_ascii=False).encode('utf-8')
        if canonical == token:
            kind = 'canonical'
        elif canonical.replace(b'/', b'\\/') == token:
            kind = 'escaped_slashes'
        elif json.dumps(value, ensure_ascii=True).encode('utf-8') == token:
            kind = 'ascii_unicode'
        else:
            kind = 'other_spelling'
        stats[kind + '_occurrences'] += 1
        stats[kind + '_value_bytes'] += len(raw)
        key = hashlib.sha256(raw).digest()
        if key not in seen:
            seen.add(key)
            stats[kind + '_unique_value_bytes'] += len(raw)
        scan(raw, depth + 1)

for i, entry in enumerate(files):
    path = Path(entry['copy'])
    proc = None
    if path.suffix == '.zst':
        proc = subprocess.Popen(['zstd', '-qdc', str(path)], stdout=subprocess.PIPE)
        source = proc.stdout
    else:
        source = path.open('rb')
    for line in source:
        stats['source_literal_backslash_slashes'] += line.count(b'\\/')
        if len(line) >= 32 * 1024 * 1024:
            stats['oversized_record_bytes'] += len(line)
            stats['oversized_records'] += 1
        scan(line)
    source.close()
    if proc and proc.wait() != 0:
        raise RuntimeError('native decode failed')
    if (i + 1) % 100 == 0:
        print(json.dumps({'files': i + 1, 'seconds': time.monotonic() - start}), flush=True)
report = {'diagnostic': 'Lexical spelling categories; nested values may overlap; not an archive size estimate',
          'files': len(files), 'stats': dict(stats), 'seconds': time.monotonic() - start}
a.report.write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report))
