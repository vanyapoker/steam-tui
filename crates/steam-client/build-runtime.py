#!/usr/bin/env python3
"""Link the Rust cdylib against an installed Steam runtime's older glibc.

Usage: build-runtime.py RUNTIME/files/lib/x86_64-linux-gnu
Only builds steam-client; does not change system libraries or Cargo defaults.
"""
import argparse
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('libraries', type=Path)
    args = parser.parse_args()
    libraries = args.libraries.resolve(strict=True)
    workspace = Path(__file__).resolve().parents[2]
    target = workspace / 'target/steam-client-runtime'
    links = target / 'link-libraries'
    links.mkdir(parents=True, exist_ok=True)
    # Runtime distributions keep the versioned ELF files but can omit the
    # development symlinks. Supply those only in the private build directory.
    for name in ('c', 'm', 'pthread', 'dl', 'rt', 'util'):
        candidates = sorted(libraries.glob('lib' + name + '-[0-9]*.so'))
        if len(candidates) != 1:
            parser.error('Expected one versioned lib' + name + ' in ' + str(libraries))
        link = links / ('lib' + name + '.so')
        if link.is_symlink():
            link.unlink()
        link.symlink_to(candidates[0])
    subprocess.run(['cargo', 'rustc', '--locked', '--release', '-p', 'steam-client', '--lib',
                    '--target-dir', str(target), '--', '-L', 'native=' + str(links)],
                   cwd=workspace, check=True)
    print(target / 'release/libsteamclient.so')


if __name__ == '__main__':
    main()
