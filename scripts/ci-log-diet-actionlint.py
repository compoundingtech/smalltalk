#!/usr/bin/env python3
"""Execute the explicit actionlint pinned inside the actual Genie closure."""
import hashlib
import os
from pathlib import Path
import re
import shutil
import sys


def resolve(wrapper):
    rows = re.findall(r"^export GENIE_ACTIONLINT_BIN='(/nix/store/[^'\n]+/bin/actionlint)'$", wrapper, re.M)
    if len(rows) != 1: raise ValueError('actual Genie wrapper does not identify one pinned actionlint')
    return rows[0]


if __name__ == '__main__':
    genie = shutil.which('genie')
    if not genie: raise SystemExit('actual Genie closure unavailable')
    tool = resolve(Path(genie).read_text())
    print('explicit-actionlint ' + tool + ' sha256=' + hashlib.sha256(Path(tool).read_bytes()).hexdigest(), file=sys.stderr, flush=True)
    os.execv(tool, [tool, *sys.argv[1:]])
