"""CI assertions for fresh st seats; predecessor compatibility and st2 stay independent."""
import re
import sqlite3
from pathlib import Path

IMPORT = re.compile(r"\b(?:r#)?st2\s*::|\bextern\s+crate\s+(?:r#)?st2\b")
LEGACY = re.compile("st2", re.IGNORECASE)
# A stored value names st2 as its own token: a label, schema, path or program. Keys, signatures,
# nonces and other base64 that st stores hold the letters st2 inside longer runs by chance.
TOKEN = re.compile(r"(?<![a-z0-9])st2(?![a-z0-9])", re.IGNORECASE)


def source_findings(root):
    for crate in ("st3", "stui"):
        for path in sorted((root / "crates" / crate).rglob("*.rs")):
            source = path.read_text()
            for match in IMPORT.finditer(source):
                number = source[:match.start()].count("\n") + 1
                yield f"{path.relative_to(root)}:{number}: st2 import"


def dependency_findings(metadata):
    # --no-deps metadata retains every workspace dependency, including aliases, optional
    # features and target/dev/build dependencies. Traverse neutral crates too: an alias or
    # indirect workspace dependency must not let st2 back into either current product.
    packages = {package["name"]: package for package in metadata["packages"]}
    for product in ("st3", "stui"):
        pending = [(product, [product])]
        seen = set()
        while pending:
            name, chain = pending.pop()
            if name in seen:
                continue
            seen.add(name)
            for dependency in packages.get(name, {}).get("dependencies", []):
                target = dependency["name"]
                if target == "st2":
                    yield "st2 dependency: " + " -> ".join([*chain, target])
                elif target in packages:
                    pending.append((target, [*chain, target]))


def launch_findings(argv, environment):
    for entry in environment:
        name, _, _ = entry.partition("=")
        if name.startswith("ST2_"):
            yield f"legacy environment name: {name}"
    # Reject any path component containing st2, not just an exact /st2/ component.
    # Bare st2 is also a program. JSON plugin-disable keys are not paths/programs.
    for text in (argv, *environment):
        for token in re.split(r"[\s\"'();|&`=:]+", text):
            if token == "st2" or ("/" in token and LEGACY.search(token)):
                yield f"st2 program/path in launch: {token}"


def record_findings(label, value, pattern=LEGACY):
    if isinstance(value, bytes):
        try:
            value = value.decode("utf-8")
        except UnicodeError:
            return
    if isinstance(value, str) and pattern.search(value):
        yield f"st2 in {label}"


def sqlite_findings(path):
    # A read-only connection sees WAL commits without writing or checkpointing the live DB.
    with sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True) as connection:
        tables = connection.execute("SELECT name,sql FROM sqlite_master WHERE type='table'").fetchall()
        for name, schema in tables:
            yield from record_findings(f"SQLite schema {path.name}/{name}", schema)
            escaped = name.replace('"', '""')
            cursor = connection.execute(f'SELECT * FROM "{escaped}"')
            columns = [column[0] for column in cursor.description]
            for row in cursor:
                for column, value in zip(columns, row):
                    # These are opaque base64, not labels. Their decoded claims are also stored
                    # in semantic tables checked below; random bytes can contain the letters st2.
                    if (name, column) in {("replica_envelopes", "payload"),
                                          ("replica_envelope_signatures", "signature"),
                                          ("claim_signatures", "signature")}:
                        continue
                    yield from record_findings(
                        f"SQLite record {path.name}/{name}/{column}", value, TOKEN
                    )


def output_findings(root):
    # Fixtures use only neutral invented labels. Check actual generated paths and text/SQLite
    # records, including nested schemas, plus logs. Never scan source or predecessor fixtures.
    inspected_databases = set()
    for directory in (root / "state", root / "home", root / "pty"):
        for path in sorted(directory.rglob("*")):
            relative = path.relative_to(root)
            if LEGACY.search(str(relative)):
                yield f"st2 in generated path: {relative}"
            if path.is_symlink():
                yield from record_findings(f"symlink target {relative}", str(path.readlink()))
                continue
            if not path.is_file():
                continue
            try:
                with path.open("rb") as stream:
                    header = stream.read(16)
            except FileNotFoundError:
                # SQLite may remove a sidecar between discovery and this open. Its database
                # records must already have been inspected; other missing outputs still fail.
                if (path.name.endswith(("-wal", "-shm"))
                        and path.with_name(path.name[:-4]) in inspected_databases):
                    continue
                raise
            if header == b"SQLite format 3\x00":
                yield from sqlite_findings(path)
                inspected_databases.add(path)
            elif not path.name.endswith(("-wal", "-shm")):
                yield from record_findings(f"record/log {relative}", path.read_bytes())
    log = root / "daemon.log"
    if log.exists():
        yield from record_findings("daemon log label", log.read_bytes())


def fresh_seat_findings(root, processes):
    if not processes:
        yield "no fresh seat processes inspected"
    for pid, argv, environment in processes:
        for finding in launch_findings(argv, environment):
            yield f"process {pid}: {finding}"
    yield from output_findings(root)
