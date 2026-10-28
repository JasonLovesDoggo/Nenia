#!/usr/bin/env python3
"""Inspect actual Codex JSONL/Zstd files; publish only aggregate measurements."""
import argparse
import collections
import ctypes
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--source-dir', type=Path, required=True)
p.add_argument('--decoded-dir', type=Path, required=True)
p.add_argument('--report', type=Path, required=True)
a = p.parse_args()
version = subprocess.check_output(["zstd", "--version"], text=True).strip()
if "v1.5.7" not in version:
    p.error("this encoder-reproduction experiment requires Zstd CLI 1.5.7")
a.decoded_dir = a.decoded_dir.resolve()
repo = Path(__file__).resolve().parents[1]
if a.decoded_dir.is_relative_to(repo):
    p.error('decoded sessions must stay outside the repository')
a.decoded_dir.mkdir(parents=True, mode=0o700, exist_ok=False)
z = ctypes.CDLL('libzstd.so.1')
z.ZSTD_getFrameContentSize.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
z.ZSTD_getFrameContentSize.restype = ctypes.c_ulonglong
class Header(ctypes.Structure):
    _fields_ = [('frameContentSize', ctypes.c_ulonglong), ('windowSize', ctypes.c_ulonglong), ('blockSizeMax', ctypes.c_uint), ('frameType', ctypes.c_int), ('headerSize', ctypes.c_uint), ('dictID', ctypes.c_uint), ('checksumFlag', ctypes.c_uint), ('reserved1', ctypes.c_uint), ('reserved2', ctypes.c_uint)]
z.ZSTD_getFrameHeader.argtypes = [ctypes.POINTER(Header), ctypes.c_void_p, ctypes.c_size_t]
z.ZSTD_getFrameHeader.restype = ctypes.c_size_t
headers = collections.Counter()
stats = collections.Counter()
records = collections.Counter()
repeated = {}
known = {'session_meta', 'response_item', 'event_msg', 'turn_context', 'compacted'}
start = time.monotonic()
for i, source in enumerate(sorted(a.source_dir.rglob('*'))):
    if not source.is_file() or not str(source).endswith(('.jsonl', '.jsonl.zst')):
        continue
    decoded = a.decoded_dir / f'session-{i:06}.jsonl'
    stats['files'] += 1
    stats['native_bytes'] += source.stat().st_size
    if source.suffix == '.zst':
        stats['zstd_files'] += 1
        with source.open('rb') as f:
            head = f.read(64)
        size = z.ZSTD_getFrameContentSize(head, len(head))
        header = Header()
        if z.ZSTD_getFrameHeader(ctypes.byref(header), head, len(head)) != 0:
            raise RuntimeError('invalid native frame header')
        headers['files'] += 1
        headers['max_window_bytes'] = max(headers['max_window_bytes'], header.windowSize)
        headers['dictionary_frames'] += int(header.dictID != 0)
        headers['checksum_frames'] += int(header.checksumFlag != 0)
        headers['over_256mib_decoded'] += int(size > 256 * 1024 ** 2)
        if size > 1024 ** 3:
            raise RuntimeError('source exceeds this diagnostic decoded-size limit')
        with decoded.open('xb') as out:
            subprocess.run(['zstd', '-qdc', str(source)], stdout=out, check=True)
        with decoded.open('rb') as inp:
            encoder = subprocess.Popen(['zstd', '-q', '-3', '--no-check', '--single-thread', f'--stream-size={size}', '-c'], stdin=inp, stdout=subprocess.PIPE)
            h = hashlib.sha256()
            encoded_size = 0
            while block := encoder.stdout.read(1024 * 1024):
                h.update(block)
                encoded_size += len(block)
            if encoder.wait() != 0:
                raise RuntimeError('re-encoding failed')
        with source.open('rb') as inp:
            original = hashlib.file_digest(inp, 'sha256').hexdigest()
        match = encoded_size == source.stat().st_size and h.hexdigest() == original
        stats['exact_reencoding_matches' if match else 'exact_reencoding_mismatches'] += 1
        if match:
            stats['exact_reencoding_native_bytes'] += encoded_size
    else:
        stats['raw_files'] += 1
        shutil.copyfile(source, decoded)
    decoded.chmod(0o600)
    stats['decoded_bytes'] += decoded.stat().st_size
    with decoded.open('rb') as inp:
        for line in inp:
            stats['records'] += 1
            if len(line) > 32 * 1024 * 1024:
                stats['oversized_records_skipped'] += 1
                stats['oversized_record_bytes_skipped'] += len(line)
                continue
            try:
                value = json.loads(line)
            except (ValueError, UnicodeError):
                stats['unparsed_records'] += 1
                continue
            kind = value.get('type') if isinstance(value, dict) else None
            records[kind if kind in known else 'other'] += len(line)
            stack = [value]
            while stack:
                node = stack.pop()
                if isinstance(node, dict):
                    stack.extend(node.values())
                elif isinstance(node, list):
                    stack.extend(node)
                elif isinstance(node, str):
                    raw = node.encode('utf-8', errors='replace')
                    if node.startswith('data:image/'):
                        stats['image_data_uri_bytes'] += len(raw)
                    if len(raw) >= 1024:
                        stats['long_string_bytes'] += len(raw)
                        key = hashlib.sha256(raw).digest()
                        if key not in repeated:
                            repeated[key] = len(raw)
    if stats['files'] % 50 == 0:
        print(json.dumps({'processed_files': stats['files']}), flush=True)
stats['unique_long_string_bytes'] = sum(repeated.values())
stats['repeated_long_string_bytes'] = stats['long_string_bytes'] - stats['unique_long_string_bytes']
report = {'corpus': 'user supplied native Mac Codex snapshot', 'measurements': dict(stats), 'frame_headers': dict(headers), 'raw_record_bytes_by_type': dict(records), 'encoder_probe': 'Zstd CLI 1.5.7, level 3, pledged content size, no checksum, single thread; full SHA-256 and length equality', 'elapsed_seconds': time.monotonic() - start, 'long_string_metric': 'Decoded UTF-8 strings >=1024 bytes; duplicate values across all fields, not a predicted archive saving', 'private_content_published': False}
a.report.parent.mkdir(parents=True, exist_ok=True)
a.report.write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report), flush=True)
