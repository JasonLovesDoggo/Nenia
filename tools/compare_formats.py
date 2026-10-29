#!/usr/bin/env python3
"""Sequential, alternating comparisons on two frozen private real-trace corpora.

Linux /usr/bin/time measures whole-process RSS. Individual Rust runs verify every
restored byte and refuse to overwrite reports. Only safe labels and aggregates
are printed. Source trees must remain fixed while this driver is running.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time
import sys

p = argparse.ArgumentParser()
for name in ['baseline-binary', 'baseline-repo', 'current-binary', 'current-repo',
             'original-corpus', 'complete-corpus', 'report-dir']:
    p.add_argument('--' + name, type=Path, required=True)
p.add_argument('--trials', type=int, default=3)
a = p.parse_args()
if not 1 <= a.trials <= 10:
    p.error('trials must be 1..10')
for key, value in vars(a).items():
    if isinstance(value, Path):
        setattr(a, key, value.resolve())
a.report_dir.mkdir(parents=True, exist_ok=False)
protocol = {
    'method': 'fresh vault per trial, frozen actual inputs, alternating candidate order, sequential processes',
    'trials_per_candidate_and_corpus': a.trials,
    'level': 9,
    'resource_scope': 'peak RSS and wall time cover complete process, including setup and correctness checks',
    'recovery': 'last current-format complete-corpus trial includes indexing, authenticated HTTP recovery, catalog-loss recovery and observed actual source growth',
    'runs': [],
}
protocol_path = a.report_dir / 'protocol.json'
env = os.environ.copy()
# The caller supplies a PATH with the intended Rust toolchain for environment metadata.
for cohort, corpus in [('original', a.original_corpus), ('complete', a.complete_corpus)]:
    for trial in range(1, a.trials + 1):
        variants = ['v1', 'v2'] if trial % 2 else ['v2', 'v1']
        for variant in variants:
            label = f'{cohort}-{variant}-{trial}'
            report = a.report_dir / label
            current = variant == 'v2'
            binary = a.current_binary if current else a.baseline_binary
            repo = a.current_repo if current else a.baseline_repo
            full = cohort == 'complete' and current and trial == a.trials
            command = [str(binary), 'run', '--private-root', str(corpus), '--report', str(report),
                       '--only-cohort', 'combined', '--only-candidate', 'nenia', '--trials', '1', '--level', '9']
            if not full:
                command += ['--skip-index', '--skip-recovery', '--skip-growth']
            resource = a.report_dir / (label + '-resource.json')
            command = ['/usr/bin/time', '-f', '{"peak_rss_kib":%M,"whole_process_wall_seconds":%e}', '-o', str(resource)] + command
            record = {'label': label, 'cohort': cohort, 'variant': variant, 'repeat': trial,
                      'order': len(protocol['runs']) + 1, 'full_recovery': full, 'status': 'running'}
            protocol['runs'].append(record)
            protocol_path.write_text(json.dumps(protocol, indent=2) + '\n')
            print(json.dumps({'starting': label, 'full_recovery': full}), flush=True)
            start = time.monotonic()
            result = subprocess.run(command, cwd=repo, env=env)
            record.update(status='complete' if result.returncode == 0 else 'failed',
                          return_code=result.returncode, driver_seconds=time.monotonic()-start)
            protocol_path.write_text(json.dumps(protocol, indent=2) + '\n')
            if result.returncode != 0:
                raise SystemExit(result.returncode)
            sample = json.loads((report / 'samples.jsonl').read_text())
            print(json.dumps({'completed': label, 'archive_bytes': sample['archive_bytes'],
                              'total_bytes': sample['total_bytes'], 'ingest_seconds': sample['elapsed_seconds'],
                              'restored_files': sample['restored_files']}), flush=True)

subprocess.run([sys.executable, str(Path(__file__).with_name('summarize_comparison.py')),
                str(a.report_dir), '--compact'], check=True)
