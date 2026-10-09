#!/usr/bin/env python3
"""Validate both release archives and emit a source/target manifest (run in dist)."""
import json
import pathlib
import sys
import tarfile

source = sys.argv[1]
targets = ('x86_64-unknown-linux-gnu', 'aarch64-apple-darwin')
records = []
for target in targets:
    package = f'smalltalk-{target}'
    with tarfile.open(f'{package}.tar.gz') as archive:
        record = json.load(archive.extractfile(f'{package}/BUILD.json'))
        assert record['source'] == source, 'artifacts came from a different source'
        assert record['target'] == target, 'wrong target in archive'
        link = archive.getmember(f'{package}/bin/st')
        assert link.issym() and link.linkname == 'st3', 'st must be a relative st3 symlink'
        for binary in ('st3', 'pty'):
            member = archive.getmember(f'{package}/bin/{binary}')
            assert member.isfile() and member.mode & 0o111, f'missing executable: {binary}'
        assert not any(name.startswith((f'{package}/bin/stui', f'{package}/bin/st3-migrate')) for name in archive.getnames()), 'retired executable in archive'
        for installer in ('install.sh', 'install-macos.py'):
            member = archive.getmember(f'{package}/{installer}')
            assert member.isfile() and member.mode & 0o111, f'missing installer: {installer}'
        assert archive.getmember(f'{package}/macos-installation.md').isfile()
        records.append(record)
assert len({record['pty_revision'] for record in records}) == 1, 'PTY pins differ'
assert len({record['tag'] for record in records}) == 1, 'tags differ'
pathlib.Path('RELEASE.json').write_text(json.dumps({'source': source, 'builds': records}, indent=2) + '\n')
print(f'Validated both release targets at {source}')
