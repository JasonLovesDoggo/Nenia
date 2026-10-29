#!/usr/bin/env python3
"""Summarize completed alternating real-corpus comparisons without private data."""
import argparse
import json
from pathlib import Path
import statistics
import re
import shutil

p = argparse.ArgumentParser()
p.add_argument('comparison', type=Path)
p.add_argument('--compact', action='store_true', help='Consolidate redundant per-run aggregate files, preserving every measurement')
a = p.parse_args()
protocol = json.loads((a.comparison / 'protocol.json').read_text())
assert all(r['status'] == 'complete' for r in protocol['runs']), 'comparison incomplete'
groups = {}
records = []
corpora = {}
compact_path = a.comparison / 'runs.jsonl'
existing = {r['run']['label']: r for r in map(json.loads, compact_path.read_text().splitlines())} if compact_path.exists() else None
for run in protocol['runs']:
    label = run['label']
    assert re.fullmatch(r'(original|complete)-v[12]-[1-9][0-9]*', label)
    if existing is not None:
        record = existing[label]
    else:
        directory = a.comparison / label
        rows = (directory / 'samples.jsonl').read_text().splitlines()
        assert len(rows) == 1
        record = {'run': run, 'sample': json.loads(rows[0]),
                  'environment': json.loads((directory / 'environment.json').read_text()),
                  'resource': json.loads((a.comparison / (label + '-resource.json')).read_text()),
                  'recovery': json.loads((directory / 'recovery.json').read_text()),
                  'observed_growth': json.loads((directory / 'observed-growth.json').read_text()),
                  'summary': json.loads((directory / 'summary.json').read_text())}
        corpus = json.loads((directory / 'corpus-aggregate.json').read_text())
        assert run['cohort'] not in corpora or corpora[run['cohort']] == corpus
        corpora[run['cohort']] = corpus
    sample, resource = record['sample'], record['resource']
    assert sample['errors'] == 0 and sample['restored_files'] == sample['files']
    groups.setdefault(run['cohort'] + '/' + run['variant'], []).append((sample, resource))
    records.append(record)
summary = {}
for key, rows in groups.items():
    assert len(rows) == protocol['trials_per_candidate_and_corpus']
    result = {'trials': len(rows), 'files': rows[0][0]['files'], 'input_bytes': rows[0][0]['input_bytes'],
              'exact_measured_restores': sum(s['restored_files'] for s, _ in rows)}
    for name in ['archive_bytes', 'payload_bytes', 'manifest_bytes', 'catalog_bytes', 'total_bytes',
                 'elapsed_seconds', 'restore_seconds', 'unchanged_capture_seconds', 'metadata_poll_seconds']:
        values = [s[name] for s, _ in rows if s.get(name) is not None]
        if values:
            result[name + '_min_median_max'] = [min(values), statistics.median(values), max(values)]
    result['whole_process_peak_rss_kib_max'] = max(r['peak_rss_kib'] for _, r in rows)
    result['unchanged_archive_growth_bytes'] = [s['unchanged_archive_growth_bytes'] for s, _ in rows]
    for name in ['structural_snapshots', 'native_reconstructed', 'native_opaque']:
        if rows[0][0].get(name) is not None:
            result[name] = rows[0][0][name]
    summary[key] = result
for cohort in ['original', 'complete']:
    before = summary[cohort + '/v1']['archive_bytes_min_median_max'][1]
    after = summary[cohort + '/v2']['archive_bytes_min_median_max'][1]
    summary[cohort + '/comparison'] = {'archive_bytes_saved': before - after,
                                    'archive_reduction_percent': 100 * (1 - after / before)}
(a.comparison / 'comparison-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
print(json.dumps(summary, indent=2))

if a.compact and existing is None:
    compact_path.write_text(''.join(json.dumps(r, separators=(',', ':')) + '\n' for r in records))
    (a.comparison / 'corpora.json').write_text(json.dumps(corpora, indent=2) + '\n')
    for record in records:
        if record['run']['full_recovery']:
            for name in ['recovery', 'observed_growth']:
                (a.comparison / (name.replace('_', '-') + '.json')).write_text(json.dumps(record[name], indent=2) + '\n')
    # These are redundant aggregate reports, never archive objects or source data.
    for run in protocol['runs']:
        directory = a.comparison / run['label']
        assert directory.resolve().parent == a.comparison.resolve()
        allowed = {'corpus-aggregate.json','environment.json','recovery.json','observed-growth.json','samples.jsonl','summary.json'}
        assert {p.name for p in directory.iterdir()} == allowed
        shutil.rmtree(directory)
        (a.comparison / (run['label'] + '-resource.json')).unlink()
