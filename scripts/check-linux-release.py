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
    symbols = subprocess.check_output(['readelf', '--dyn-syms', '--wide', path], text=True)
    versions = re.findall(r'@GLIBC_(\d+(?:\.\d+)+)', symbols)
    newer = [v for v in versions if tuple(map(int, v.split('.'))) > (2, 35)]
    if newer:
        raise SystemExit(f'{path}: requires GLIBC_{max(newer, key=lambda v: tuple(map(int, v.split("."))))}; baseline is 2.35')
print('Linux release libc baseline verified')
