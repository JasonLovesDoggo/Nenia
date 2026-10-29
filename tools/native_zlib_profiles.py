#!/usr/bin/env python3
"""Byte-prove C zlib reconstruction profiles on private, actual PNG streams."""
import collections
import json
from pathlib import Path
import struct
import sys
import time
import zlib

root, output = map(Path, sys.argv[1:3])
assets = json.loads((root / 'assets.json').read_text())
stats = collections.Counter()
samples = []
start = time.monotonic()
for ordinal, asset in enumerate(assets):
    if asset['kind'] != 'png':
        continue
    png = (root / asset['file']).read_bytes()
    chunks = []
    pos = 8
    try:
        while pos < len(png):
            length, kind = struct.unpack('>I4s', png[pos:pos + 8])
            if pos + 12 + length > len(png):
                raise ValueError('truncated')
            if kind == b'IDAT':
                chunks.append(png[pos + 8:pos + 8 + length])
            pos += length + 12
        encoded = b''.join(chunks)
        raw = zlib.decompress(encoded)
        level_hint = encoded[1] >> 6
    except (ValueError, zlib.error, struct.error, IndexError):
        stats['unsupported_input'] += 1
        continue
    levels = {0: [1, 0], 1: [3, 2], 2: [6, 5, 4], 3: [9, 8, 7]}[level_hint]
    profile = None
    attempts = 0
    strategies = [zlib.Z_DEFAULT_STRATEGY, zlib.Z_FILTERED]
    if level_hint == 0:
        strategies += [zlib.Z_RLE, zlib.Z_HUFFMAN_ONLY, zlib.Z_FIXED]
    for strategy in strategies:
        for level in levels:
            for memory in [8, 9]:
                attempts += 1
                compressor = zlib.compressobj(level, zlib.DEFLATED, 15, memory, strategy)
                candidate = compressor.compress(raw) + compressor.flush()
                if candidate == encoded:
                    profile = [level, memory, strategy]
                    break
            if profile:
                break
        if profile:
            break
    stats[str(profile)] += 1
    samples.append({'ordinal': ordinal, 'cohort': asset['cohort'],
                    'native_zlib_bytes': len(encoded), 'profile': profile, 'attempts': attempts})
    if len(samples) % 50 == 0:
        print(json.dumps({'images': len(samples), 'seconds': time.monotonic() - start}), flush=True)
report = {'candidate': 'native-C-zlib-profile-proof', 'runtime_version': zlib.ZLIB_RUNTIME_VERSION,
          'build_version': zlib.ZLIB_VERSION, 'stats': dict(stats), 'samples': samples,
          'seconds': time.monotonic() - start}
output.write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps({k: v for k, v in report.items() if k != 'samples'}))
