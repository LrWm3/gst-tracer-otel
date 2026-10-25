"""Measure cold/warm native symbol resolution in fresh debug-build processes.

Build: cargo test --profile profiling -p gst-otel-tracer --features profiles --lib --no-run
Pass the resulting test executable with --binary. No compilation occurs here.
"""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--cpus', default='0,2')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    binary = args.binary.resolve()
    metadata = dict(architecture=platform.machine(), repeats=args.repeats, cpus=args.cpus,
                    binary_bytes=binary.stat().st_size,
                    binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                    source_sha256={str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                                   for p in map(Path, ['tracer/otel/src/profilesymbols.rs',
                                                       'tracer/otel/src/profiles.rs',
                                                       'tracer/otel/bench/symbolization.py', 'Cargo.lock'])},
                    revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                    phase_meanings=dict(cold='empty resolver cache, OS page cache not flushed',
                                        warm='same six PCs with the retained cache',
                                        cache_cleared='same PCs after explicit whole-cache invalidation'))
    (args.output / 'metadata.json').write_text(json.dumps(metadata, indent=2) + '\n')
    rows = []
    base = {k: v for k, v in os.environ.items() if not k.startswith(('MARS_', 'GST_', 'OTEL_'))}
    for repeat in range(args.repeats):
        for mode in (('full', 'functions') if repeat % 2 == 0 else ('functions', 'full')):
            env = dict(base, MARS_PROFILE_SYMBOLIZATION=mode)
            result = subprocess.run(['taskset', '-c', args.cpus, str(binary), '--exact',
                                     'profiles::symbols::tests::symbolization_measurement',
                                     '--ignored', '--nocapture'], env=env, capture_output=True, text=True, timeout=60)
            assert result.returncode == 0, result.stdout + result.stderr
            (args.output / f'{mode}-{repeat}.txt').write_text(result.stdout)
            matched = re.findall(r'^SYMBOLS (.+)$', result.stdout, re.M)
            assert len(matched) == 3, result.stdout
            for line in matched:
                row = dict(repeat=repeat, mode=mode)
                for key, value in re.findall(r'(\w+)=(\S+)', line):
                    row[key] = int(value) if value.isdigit() else value
                rows.append(row)
                print(row, flush=True)
            with (args.output / 'results.csv').open('w') as stream:
                writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
                writer.writeheader()
                writer.writerows(rows)


if __name__ == '__main__':
    main()
