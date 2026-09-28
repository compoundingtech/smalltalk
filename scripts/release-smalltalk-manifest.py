#!/usr/bin/env python3
"""Validate release archives and emit a source/target manifest (run in dist)."""
import json
import pathlib
import sys
import tarfile

source = sys.argv[1]
targets = ('x86_64-unknown-linux-gnu',)
records = []
for target in targets:
    package = f'smalltalk-{target}'
    with tarfile.open(f'{package}.tar.gz') as archive:
        record = json.load(archive.extractfile(f'{package}/BUILD.json'))
        assert record['source'] == source, 'artifacts came from a different source'
        assert record['target'] == target, 'wrong target in archive'
        link = archive.getmember(f'{package}/bin/st')
        assert link.issym() and link.linkname == 'st3', 'st must be a relative st3 symlink'
        for binary in ('st3', 'stui', 'st3-migrate', 'pty'):
            member = archive.getmember(f'{package}/bin/{binary}')
            assert member.isfile() and member.mode & 0o111, f'missing executable: {binary}'
        records.append(record)
assert len({record['pty_revision'] for record in records}) == 1, 'PTY pins differ'
assert len({record['tag'] for record in records}) == 1, 'tags differ'
pathlib.Path('RELEASE.json').write_text(json.dumps({'source': source, 'builds': records}, indent=2) + '\n')
print(f'Validated release targets at {source}')
