#!/usr/bin/env python3
"""Reject Linux archive binaries that require a newer libc or a build-machine loader."""
import re
import subprocess
import sys

for path in sys.argv[1:]:
    headers = subprocess.check_output(['readelf', '--program-headers', path], text=True)
    if not re.search(r'Requesting program interpreter: /(lib64|lib/x86_64-linux-gnu)/ld-linux-x86-64.so.2', headers):
        raise SystemExit(f'{path}: expected the native Linux loader')
    dynamic = subprocess.check_output(['readelf', '--dynamic', path], text=True)
    if '/nix/store/' in dynamic or '/nix/store/' in headers:
        raise SystemExit(f'{path}: build-machine runtime path in release')
    # Inspect the artifact instead of the build host's ldd, which may itself use
    # a Nix loader. These libraries are present on the supported Ubuntu baseline.
    needed = set(re.findall(r'Shared library: \[(.*?)\]', dynamic))
    standard = {'libc.so.6', 'libm.so.6', 'libpthread.so.0', 'libdl.so.2',
                'librt.so.1', 'libgcc_s.so.1', 'ld-linux-x86-64.so.2'}
    if needed - standard:
        raise SystemExit(f'{path}: unsupported runtime libraries: {sorted(needed - standard)}')
    symbols = subprocess.check_output(['readelf', '--dyn-syms', '--wide', path], text=True)
    versions = re.findall(r'@GLIBC_(\d+(?:\.\d+)+)', symbols)
    newer = [v for v in versions if tuple(map(int, v.split('.'))) > (2, 35)]
    if newer:
        raise SystemExit(f'{path}: requires GLIBC_{max(newer, key=lambda v: tuple(map(int, v.split("."))))}; baseline is 2.35')
print('Linux release libc baseline verified')
