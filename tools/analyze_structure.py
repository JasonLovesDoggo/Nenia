#!/usr/bin/env python3
"""Aggregate-only structural inventory of a private real_benchmark corpus.

This is a diagnostic, not an archive implementation or performance benchmark.
Decoded values written under --private-root are sensitive and never published.
"""
import argparse
import base64
import collections
import hashlib
import json
from pathlib import Path
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
files.sort(key=lambda f: (f['provider'] == 'additional_claude', f['provider'], f['copy']))
seen = set()
media_seen = set()
records_seen = set()
stats = collections.Counter()
groups = collections.defaultdict(collections.Counter)
streams = {}
start = time.monotonic()

def emit(kind, raw):
    if kind not in streams:
        streams[kind] = (a.private_root / (kind + '.bin')).open('xb')
    streams[kind].write(raw)

for i, entry in enumerate(files):
    source = Path(entry['copy'])
    proc = None
    if source.suffix == '.zst':
        proc = subprocess.Popen(['zstd', '-qdc', str(source)], stdout=subprocess.PIPE)
        inp = proc.stdout
    else:
        inp = source.open('rb')
    for line in inp:
        stats['decoded_bytes'] += len(line)
        stats['records'] += 1
        key = hashlib.sha256(line).digest()
        if key not in records_seen:
            records_seen.add(key)
            stats['unique_record_bytes'] += len(line)
        try:
            value = json.loads(line)
        except (ValueError, UnicodeError):
            stats['unparsed_record_bytes'] += len(line)
            continue
        stack = [(None, value)]
        while stack:
            field, node = stack.pop()
            if isinstance(node, dict):
                stack.extend(node.items())
            elif isinstance(node, list):
                stack.extend((field, x) for x in node)
            elif isinstance(node, str):
                raw = node.encode('utf-8', errors='surrogatepass')
                if len(raw) < 128:
                    stats['short_string_value_bytes'] += len(raw)
                    continue
                kind = 'text'
                binary = None
                if node.startswith('data:image/') and ';base64,' in node[:80]:
                    kind = 'image'
                    try:
                        binary = base64.b64decode(node.split(';base64,',1)[1], validate=True)
                    except ValueError:
                        stats['invalid_image_base64'] += 1
                elif field == 'encrypted_content':
                    kind = 'encrypted'
                elif field == 'data':
                    try:
                        binary = base64.b64decode(node, validate=True)
                        kind = 'base64'
                    except ValueError:
                        pass
                g = groups[kind]
                g['occurrences'] += 1
                g['bytes'] += len(raw)
                key = hashlib.sha256(raw).digest()
                if key not in seen:
                    seen.add(key)
                    g['unique_values'] += 1
                    g['unique_bytes'] += len(raw)
                    emit(kind + '_strings', raw + b'\n')
                if binary is not None:
                    g['binary_bytes'] += len(binary)
                    key = hashlib.sha256(binary).digest()
                    if key not in media_seen:
                        media_seen.add(key)
                        g['unique_binary_bytes'] += len(binary)
                        fmt = 'png' if binary.startswith(b'\x89PNG') else 'jpeg' if binary.startswith(b'\xff\xd8\xff') else 'other'
                        g['unique_' + fmt + '_bytes'] += len(binary)
                        emit('media_' + fmt, binary)
    inp.close()
    if proc and proc.wait() != 0:
        raise RuntimeError('source decode failed')
    stats['files'] += 1
    if i == 341 or (i+1) % 50 == 0 or i+1 == len(files):
        print(json.dumps({'files':i+1,'elapsed_seconds':round(time.monotonic()-start,2),'stats':dict(stats),'groups':{k:dict(v) for k,v in groups.items()}}),flush=True)
        if i == 341:
            old = {'stats':dict(stats),'groups':{k:dict(v) for k,v in groups.items()}}
for f in streams.values():
    f.close()
report = {'diagnostic':'Decoded string values >=128 bytes; exact SHA-256 deduplication; not an archive size estimate','original_342':old,'combined':{'stats':dict(stats),'groups':{k:dict(v) for k,v in groups.items()}},'elapsed_seconds':time.monotonic()-start,'private_content_published':False}
a.report.parent.mkdir(parents=True,exist_ok=True)
a.report.write_text(json.dumps(report,indent=2)+'\n')
