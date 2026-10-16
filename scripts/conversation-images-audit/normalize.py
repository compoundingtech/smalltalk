#!/usr/bin/env python3
"""Normalize captured timelines into client fixtures.

Usage: normalize.py AUDIT_DIR OUT_DIR TIMELINE.json...

Keeps each response's `value` (a timeline page). Replaces the signed content references with
short invented ones (`v2.audit-<file>-<n>`), the audit directory with /work/garden, and the
instance host ids with the invented names garden and pond. Writes OUT_DIR/<name>.json, where
<name> is the part of the file name after `timeline-`.
"""

import json
import re
import sys
from pathlib import Path


def main():
    audit, out = sys.argv[1], Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    for path in map(Path, sys.argv[3:]):
        name = path.stem.removeprefix("timeline-")
        value = json.loads(path.read_text())["value"]
        refs = {}

        def ref(match):
            token = match.group(1)
            refs.setdefault(token, f"v2.audit-{name}-{len(refs) + 1}")
            return f'"ref": "{refs[token]}"'

        text = json.dumps(value, indent=1, ensure_ascii=False)
        text = re.sub(r'"ref": "(v2\.[A-Za-z0-9_\-]+)"', ref, text)
        text = text.replace(audit, "/work/garden")
        text = re.sub(r"garden-host-images-audit", "garden", text)
        text = re.sub(r"pond-host-images-audit-pond", "pond", text)
        (out / f"{name}.json").write_text(text + "\n")
        print(out / f"{name}.json", len(value.get("items", [])), "items,", len(refs), "references")


if __name__ == "__main__":
    main()
