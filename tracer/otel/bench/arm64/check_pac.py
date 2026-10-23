"""Exercise actual PAC signatures under QEMU; not a CPU-sampler/timing test.

Run in a native arm64 development environment with qemu-aarch64 installed.
Native timer, unwinding, and shutdown tests must be run separately on hardware.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--qemu', default='qemu-aarch64')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[4]
    build = subprocess.run([
        'cargo', 'test', '--locked', '-p', 'gst-otel-tracer', '--features', 'profiles',
        '--lib', '--no-run', '--message-format=json',
    ], cwd=root, capture_output=True, text=True)
    if build.returncode:
        print(build.stderr, file=sys.stderr, end='')
        build.check_returncode()
    binaries = [m['executable'] for line in build.stdout.splitlines()
                if (m := json.loads(line)).get('executable')
                and m.get('target', {}).get('name') == 'gstoteltracer']
    assert len(binaries) == 1, binaries
    result = subprocess.run([
        args.qemu, '-cpu', 'max', binaries[0], '--exact',
        'profiles::arch::tests::signed_and_unsigned_return_addresses_resolve_to_the_same_callsite',
        '--nocapture',
    ], cwd=root, capture_output=True, text=True, timeout=60)
    print(result.stdout, end='')
    print(result.stderr, file=sys.stderr, end='')
    result.check_returncode()
    assert '1 passed' in result.stdout, 'PAC test was not compiled for this target'
    assert 'key A=true, key B=true' in result.stdout, 'QEMU did not exercise both PAC keys'


if __name__ == '__main__':
    main()
