"""Lexical SQLite source-site inventory, deliberately not a Rust/SQL type or size proof."""
from __future__ import annotations

from bisect import bisect_right
from collections import Counter
from dataclasses import dataclass
import hashlib
import json
from pathlib import Path
import re

MANIFEST = "docs/st3/large-value-sites.json"
MARKER = re.compile(
    r"/\* sqlite-large-value: allow-for-now (?P<id>lv-h[0-9a-f]{12}) "
    r"owner=(?P<owner>[a-zA-Z0-9_:]+) TODO=(?P<todo>[a-z-]+) \*/"
)
ACTIONS = {
    "schema": "chunk-or-reference-variable-columns",
    "claim": "prepare-body-and-predecessor-values",
    "wire": "chunk-or-reference-exact-wire-values",
    "checkpoint": "stage-generation-and-keyed-reclamation",
    "dynamic": "trace-statement-and-value-representation",
    "other": "bound-or-reference-values-and-reclamation",
}
# Matches statement templates, including trigger bodies and CTE statements. It does not
# identify SQL assembled only from fragments/variables: those compilation/execution sinks
# are separately inventoried. SQL text inside diagnostics may be conservatively included.
WRITE = re.compile(
    r"\b(?:INSERT\s+(?:OR\s+\w+\s+)?INTO|REPLACE\s+INTO|DELETE\s+FROM|"
    r"UPDATE\s+(?:OR\s+\w+\s+)?[\w\"`{][\s\S]*?\bSET\b|"
    r"CREATE\s+(?:(?:TEMP|TEMPORARY|UNIQUE)\s+)*(?:TABLE|INDEX|TRIGGER|VIEW)|"
    r"ALTER\s+TABLE|DROP\s+(?:TABLE|INDEX|TRIGGER|VIEW))\b", re.I
)
METHODS = {"execute", "execute_batch", "prepare", "prepare_cached", "query_row"}
IDENT = re.compile(r"[A-Za-z_][A-Za-z_0-9]*")
RAW = re.compile(r'(?:br|cr|r)(#*)"')
CHAR = re.compile(r"(?:b)?'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'")


@dataclass(frozen=True)
class Token:
    kind: str
    text: str
    start: int
    end: int


@dataclass(frozen=True)
class Site:
    path: str
    kind: str
    start: int
    end: int
    line: int
    fingerprint: str
    id: str
    owner: str
    todo: str
    excerpt: str


def tokens(source: str) -> list[Token]:
    """Preserve literal/comment spans; skip neither raw strings nor nested comments."""
    result = []
    i = 0
    while i < len(source):
        start = i
        if source[i].isspace():
            i += 1
            continue
        if source.startswith("//", i):
            i = source.find("\n", i)
            if i < 0:
                i = len(source)
            result.append(Token("comment", source[start:i], start, i))
            continue
        if source.startswith("/*", i):
            i += 2
            depth = 1
            while i < len(source) and depth:
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError(f"unterminated block comment at offset {start}")
            result.append(Token("comment", source[start:i], start, i))
            continue
        raw = RAW.match(source, i)
        if raw:
            close = '"' + raw.group(1)
            i = raw.end()
            end = source.find(close, i)
            if end < 0:
                raise ValueError(f"unterminated raw literal at offset {start}")
            i = end + len(close)
            result.append(Token("string", source[start:i], start, i))
            continue
        prefix = 1 if source[i] == '"' else 2 if source[i:i + 2] in ('b"', 'c"') else 0
        if prefix:
            i += prefix
            while i < len(source):
                if source[i] == "\\":
                    i += 2
                elif source[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            else:
                raise ValueError(f"unterminated literal at offset {start}")
            result.append(Token("string", source[start:i], start, i))
            continue
        # Character literals must not expose braces or comment-looking bytes. Lifetimes
        # ('a / 'static) remain punctuation + identifier instead of swallowing the file.
        char = CHAR.match(source, i)
        if char:
            i = char.end()
            result.append(Token("char", source[start:i], start, i))
            continue
        ident = IDENT.match(source, i)
        if ident:
            i = ident.end()
            result.append(Token("ident", source[start:i], start, i))
            continue
        i += 1
        result.append(Token("punct", source[start:i], start, i))
    return result


def literal_text(token: Token) -> str:
    raw = re.match(r'(?:br|cr|r)(#*)"', token.text)
    if raw:
        return token.text[len(raw.group(0)):-(1 + len(raw.group(1)))]
    text = token.text[1:] if token.text[:1] in "bc" else token.text
    try:
        return json.loads(text)
    except (ValueError, TypeError):
        # Rust-specific escapes/continuations are not evaluated as executable source.
        # Keep a conservative searchable lexical body and expose this limitation.
        return text[1:-1].replace('\\"', '"').replace("\\n", "\n")


def paired(ts: list[Token], start: int) -> int:
    opening = ts[start].text
    closing = {"(": ")", "[": "]", "{": "}"}[opening]
    stack = [closing]
    for i in range(start + 1, len(ts)):
        if ts[i].kind in ("string", "char", "comment"):
            continue
        text = ts[i].text
        if text in ("(", "[", "{"):
            stack.append({"(": ")", "[": "]", "{": "}"}[text])
        elif text in (")", "]", "}"):
            if not stack or text != stack.pop():
                raise ValueError(f"unbalanced Rust delimiter at offset {ts[i].start}")
            if not stack:
                return i
    raise ValueError(f"unclosed Rust delimiter at offset {ts[start].start}")


def test_ranges(ts: list[Token]) -> list[tuple[int, int]]:
    """Exclude items explicitly #[cfg(test)] only, not not(test)/feature conditions."""
    ranges = []
    i = 0
    while i + 7 < len(ts):
        if [t.text for t in ts[i:i + 7]] != ["#", "[", "cfg", "(", "test", ")", "]"]:
            i += 1
            continue
        j = i + 7
        # Walk attributes/signature to the item's top-level body/semicolon. This is a
        # lexical item boundary, not evaluation of arbitrary cfg expressions/macros.
        while j < len(ts):
            if ts[j].text in ("(", "["):
                j = paired(ts, j) + 1
            elif ts[j].text == "{":
                end = paired(ts, j)
                ranges.append((ts[i].start, ts[end].end))
                i = end + 1
                break
            elif ts[j].text == ";":
                ranges.append((ts[i].start, ts[j].end))
                i = j + 1
                break
            else:
                j += 1
        else:
            raise ValueError("cfg(test) item has no lexical end")
    return ranges


def owner_for(path: str) -> str:
    parts = Path(path).parts
    crate = parts[1].replace("-", "_")
    relative = list(parts[3:])
    if relative:
        relative[-1] = Path(relative[-1]).stem
    if relative and relative[-1] in ("lib", "main", "mod"):
        relative.pop()
    return "::".join([crate, *relative])


def todo_for(text: str, kind: str) -> str:
    lower = text.lower()
    if kind != "sql-template":
        return ACTIONS["dynamic"]
    if re.search(r"\bcreate\s+(?:(?:temp|temporary|unique)\s+)*(?:table|index)", lower):
        return ACTIONS["schema"]
    if "checkpoint" in lower:
        return ACTIONS["checkpoint"]
    if "replica" in lower or "envelope" in lower:
        return ACTIONS["wire"]
    if re.search(r"\bclaims\b", lower):
        return ACTIONS["claim"]
    return ACTIONS["other"]


def scan_file(path: str, source: str, track_dynamic: bool = True) -> tuple[list[Site], list[Token]]:
    all_tokens = tokens(source)
    ts = [t for t in all_tokens if t.kind != "comment"]
    excluded = test_ranges(ts)
    active = [t for t in ts if not any(a <= t.start < b for a, b in excluded)]
    candidates = []
    sql_spans = []
    for t in active:
        if t.kind == "string" and WRITE.search(literal_text(t)):
            candidates.append(("sql-template", t.start, t.end, t.text, literal_text(t)))
            sql_spans.append((t.start, t.end))
    for i, t in enumerate(active):
        # These may include non-SQL execute methods. Over-inclusion is deliberate;
        # the manifest labels ambiguous sinks instead of guessing a Rust receiver type.
        if track_dynamic and i and active[i - 1].text == "." and t.text in METHODS and i + 1 < len(active) and active[i + 1].text == "(":
            end = paired(active, i + 1)
            args = active[i + 2:end]
            depth = 0
            first_arg = []
            for arg in args:
                if arg.text == "," and depth == 0:
                    break
                first_arg.append(arg)
                if arg.kind not in ("string", "char"):
                    depth += arg.text in ("(", "[", "{")
                    depth -= arg.text in (")", "]", "}")
            strings = [s for s in first_arg if s.kind == "string"]
            if any(a <= s.start < b for s in strings for a, b in sql_spans):
                continue
            # A direct read-only literal/template is not a new write candidate. This
            # is lexical, not protection against malicious interpolation of SQL verbs.
            direct = first_arg and (first_arg[0].kind == "string" or
                                    first_arg[0].text in ("format", "concat"))
            if direct and strings and re.match(r"\s*(?:SELECT\b|EXPLAIN\b|PRAGMA\b)", literal_text(strings[0]), re.I):
                continue
            start = args[0].start if args else active[end].start
            finish = args[-1].end if args else start
            normalized = " ".join(s.text for s in args)
            candidates.append(("unresolved-sql-sink", start, finish, normalized, normalized))
        if (t.text == "include" or (track_dynamic and t.text == "include_str")) and i + 2 < len(active) and active[i + 1].text == "!" and active[i + 2].text == "(":
            end = paired(active, i + 2)
            text = " ".join(s.text for s in active[i:end + 1])
            candidates.append(("included-source", t.start, active[end].end, text, text))
    counts = Counter()
    sites = []
    for kind, start, end, raw, excerpt in sorted(candidates, key=lambda x: (x[1], x[0])):
        fingerprint = hashlib.sha256(raw.encode()).hexdigest()
        key = (kind, fingerprint)
        counts[key] += 1
        identifier = "lv-h" + hashlib.sha256(f"{path}\0{kind}\0{fingerprint}\0{counts[key]}".encode()).hexdigest()[:12]
        sites.append(Site(path, kind, start, end, source.count("\n", 0, start) + 1,
                          fingerprint, identifier, owner_for(path), todo_for(excerpt, kind),
                          re.sub(r"\s+", " ", excerpt)[:200]))
    return sites, all_tokens


def source_paths(root: Path):
    # New crates/source files enter automatically. Integration tests, fixtures and
    # arbitrary generated OUT_DIR files are outside declared coverage, not green proof.
    return sorted(p for p in (root / "crates").rglob("*.rs") if "src" in p.relative_to(root / "crates").parts)


def record(site: Site) -> dict:
    return {"id": site.id, "path": site.path, "kind": site.kind,
            "fingerprint": site.fingerprint, "owner": site.owner, "todo": site.todo,
            "disposition": "allow-for-now", "line_hint": site.line, "excerpt": site.excerpt}


def scan(root: Path, cache: dict | None = None):
    sites = []
    markers = []
    dependencies = set(sqlite_crates(root))
    for path in source_paths(root):
        relative = path.relative_to(root).as_posix()
        source = path.read_text()
        found, ts = scan_file(relative, source, Path(relative).parts[1] in dependencies)
        if cache is not None:
            cache[relative] = (source, ts)
        sites.extend(found)
        for t in ts:
            if t.kind != "comment" or "sqlite-large-value:" not in t.text:
                continue
            match = MARKER.fullmatch(t.text)
            markers.append((relative, t.start, t.end, match.groupdict() if match else None))
    return sites, markers


def findings(root: Path, manifest: dict) -> list[str]:
    problems = []
    if manifest.get("version") != 1 or manifest.get("coverage") != "rust-source-sql-sites-v1":
        problems.append("unsupported/malformed inventory version or coverage")
    entries = manifest.get("sites")
    if not isinstance(entries, list):
        return [*problems, "inventory sites must be a list"]
    by_id = {}
    for e in entries:
        if not isinstance(e, dict) or not isinstance(e.get("id"), str):
            problems.append("malformed inventory entry")
            continue
        if e["id"] in by_id:
            problems.append(f"duplicate inventory exception {e['id']}")
        by_id[e["id"]] = e
    cache = {}
    sites, markers = scan(root, cache)
    if manifest.get("sqlite_crates", []) != sqlite_crates(root):
        problems.append("SQLite dependency crate inventory changed; review coverage")
    if manifest.get("declarations", []) != declarations(root, sites, {p: v[0] for p, v in cache.items()}):
        problems.append("stale/malformed potential-column/index catalog")
    marker_ids = Counter(m[3]["id"] for m in markers if m[3])
    for path, start, end, marker in markers:
        if marker is None:
            problems.append(f"{path}: malformed sqlite-large-value exception near offset {start}")
        elif marker_ids[marker["id"]] > 1:
            problems.append(f"{path}: duplicate marker {marker['id']}")
    seen_markers = set()
    contents = {p: v[0] for p, v in cache.items()}
    path_markers = {}
    for marker in markers:
        path_markers.setdefault(marker[0], []).append(marker)
    marker_ends = {p: [m[2] for m in ms] for p, ms in path_markers.items()}
    for site in sites:
        e = by_id.pop(site.id, None)
        if e is None:
            problems.append(f"{site.path}:{site.line}: unregistered {site.kind} {site.id}")
        elif any(e.get(k) != v for k, v in record(site).items() if k not in ("line_hint", "excerpt")):
            problems.append(f"{site.path}:{site.line}: stale/malformed inventory metadata for {site.id}")
        index = bisect_right(marker_ends.get(site.path, []), site.start) - 1
        marker = path_markers[site.path][index] if index >= 0 else None
        if marker and contents[site.path][marker[2]:site.start].strip():
            marker = None
        if marker is None or marker[3] is None or marker[3] != {"id": site.id, "owner": site.owner, "todo": site.todo}:
            problems.append(f"{site.path}:{site.line}: missing/mismatched adjacent allow-for-now marker for {site.id}")
        else:
            seen_markers.add((marker[0], marker[1]))
    for e in by_id.values():
        problems.append(f"stale inventory exception {e['id']}: no covered source site")
    for path, start, _, marker in markers:
        if marker is not None and (path, start) not in seen_markers:
            problems.append(f"{path}: stale/orphan marker {marker['id']}")
    flow_ids = set()
    expected_flow_markers = set()
    active_flow_tokens = {}
    for flow in manifest.get("flows", []):
        if not isinstance(flow, dict) or not all(isinstance(flow.get(k), str) for k in
                                               ("id", "path", "function", "owner", "fingerprint", "todo")):
            problems.append("malformed manual flow entry")
            continue
        if flow["id"] in flow_ids:
            problems.append(f"duplicate flow exception {flow['id']}")
        flow_ids.add(flow["id"])
        if flow["path"] not in cache:
            problems.append(f"stale/out-of-scope flow path {flow['path']}")
            continue
        source, raw_tokens = cache[flow["path"]]
        if flow["path"] not in active_flow_tokens:
            ts = [t for t in raw_tokens if t.kind != "comment"]
            excluded = test_ranges(ts)
            active_flow_tokens[flow["path"]] = [t for t in ts if not any(a <= t.start < b for a, b in excluded)]
        try:
            current = flow_site(flow["path"], source, flow["function"], active_flow_tokens[flow["path"]])
        except ValueError as error:
            problems.append(str(error))
            continue
        if (any(flow[k] != current[k] for k in ("id", "owner", "fingerprint"))
                or flow["todo"] not in ACTIONS.values() or flow.get("disposition") != "allow-for-now"):
            problems.append(f"stale/malformed flow exception {flow['id']}")
        expected = {"id": current["id"], "owner": current["owner"], "todo": flow["todo"]}
        before = [t for t in raw_tokens if t.kind == "comment" and t.end <= current["start"]]
        last = before[-1] if before else None
        marker = FLOW_MARKER.fullmatch(last.text) if last and not source[last.end:current["start"]].strip() else None
        if marker is None or marker.groupdict() != expected:
            problems.append(f"{flow['path']}: missing/malformed adjacent flow marker {flow['id']}")
        expected_flow_markers.add((flow["path"], current["id"]))
    for relative, (_, raw_tokens) in cache.items():
        for token in raw_tokens:
            if token.kind == "comment" and "sqlite-large-value-flow:" in token.text:
                marker = FLOW_MARKER.fullmatch(token.text)
                if marker is None or (relative, marker["id"]) not in expected_flow_markers:
                    problems.append(f"{relative}: stale/malformed flow marker")
    for sql in (root / "crates").rglob("*.sql"):
        problems.append(f"{sql.relative_to(root)}: standalone SQL is outside v1 coverage; extend scanner and inventory before adding it")
    return sorted(set(problems))


def sqlite_crates(root: Path) -> list[str]:
    return sorted(p.parent.name for p in (root / "crates").glob("*/Cargo.toml")
                  if re.search(r"\b(?:rusqlite|libsqlite3-sys|sqlx)\b", p.read_text()))


def split_columns(text: str) -> list[str]:
    """Split a declared SQL list lexically; unresolved templates stay unresolved."""
    result = []
    depth = 0
    quote = None
    start = 0
    i = 0
    while i < len(text):
        char = text[i]
        if quote:
            if char == quote:
                if i + 1 < len(text) and text[i + 1] == quote:
                    i += 2
                    continue
                quote = None
        elif char in "\"'`":
            quote = char
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif char == "," and depth == 0:
            result.append(text[start:i].strip())
            start = i + 1
        i += 1
    result.append(text[start:].strip())
    return result


def declarations(root: Path, sites: list[Site], contents: dict | None = None) -> list[dict]:
    result = []
    if contents is None:
        contents = {p: (root / p).read_text() for p in {s.path for s in sites}}
    for site in sites:
        if site.kind != "sql-template":
            continue
        text = literal_text(Token("string", contents[site.path][site.start:site.end], 0, 0))
        # Comments in a schema declaration are not column definitions. This is still
        # a candidate catalog, not a complete SQLite grammar or evaluated format! output.
        text = re.sub(r"--[^\n]*", "", text)
        for match in re.finditer(r"\bCREATE\s+(?:(?:TEMP|TEMPORARY)\s+)?TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([^\s(]+)\s*\(", text, re.I):
            tail = text[match.end():]
            depth, quote, end = 1, None, None
            i = 0
            while i < len(tail):
                char = tail[i]
                if quote:
                    if char == quote:
                        if i + 1 < len(tail) and tail[i + 1] == quote:
                            i += 2
                            continue
                        quote = None
                elif char in "\"'`":
                    quote = char
                elif char == "(":
                    depth += 1
                elif char == ")":
                    depth -= 1
                    if depth == 0:
                        end = i
                        break
                i += 1
            columns = []
            unresolved = end is None
            for part in split_columns(tail[:end] if end is not None else ""):
                if not part or re.match(r"(?:PRIMARY|FOREIGN|UNIQUE|CHECK|CONSTRAINT)\b", part, re.I):
                    continue
                column = re.match(r'["`]?([A-Za-z_][A-Za-z_0-9]*)["`]?\s*(.*)', part, re.S)
                if not column:
                    unresolved = True
                    continue
                declaration = re.sub(r"\s+", " ", column.group(2)).strip()
                declared_type = declaration.split(" ", 1)[0].upper() if declaration else "UNSPECIFIED"
                if declared_type in ("NOT", "PRIMARY", "REFERENCES", "DEFAULT", "CHECK", "UNIQUE", "COLLATE"):
                    declared_type = "UNSPECIFIED"
                columns.append({"name": column.group(1), "declared_type": declared_type,
                                "declaration": declaration,
                                "value_size_proven": False})
            result.append({"site": site.id, "path": site.path,
                           "table_pattern": match.group(1), "columns": columns,
                           "unresolved_template": unresolved or "{" in match.group(1)})
        for match in re.finditer(r"\bALTER\s+TABLE\s+([^\s]+)\s+ADD\s+(?:COLUMN\s+)?([^\s]+)\s+([^;]*)", text, re.I):
            name = match.group(2).strip('"`')
            declaration = re.sub(r"\s+", " ", match.group(3)).strip()
            declared_type = declaration.split(" ", 1)[0].upper() if declaration else "UNSPECIFIED"
            if declared_type in ("NOT", "PRIMARY", "REFERENCES", "DEFAULT", "CHECK", "UNIQUE", "COLLATE"):
                declared_type = "UNSPECIFIED"
            result.append({"site": site.id, "path": site.path,
                           "table_pattern": match.group(1), "operation": "add-column",
                           "columns": [{"name": name, "declared_type": declared_type,
                                        "declaration": declaration, "value_size_proven": False}],
                           "unresolved_template": "{" in match.group(1) or not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*", name)})
        for match in re.finditer(r"\bCREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:IF\s+NOT\s+EXISTS\s+)?([^\s]+)\s+ON\s+([^\s(]+)\s*(\([\s\S]*?)(?:;|$)", text, re.I):
            result.append({"site": site.id, "path": site.path,
                           "index_pattern": match.group(1), "table_pattern": match.group(2),
                           "key_expression_and_predicate": re.sub(r"\s+", " ", match.group(3)).strip(),
                           "value_size_proven": False})
    return result


FLOW_MARKER = re.compile(
    r"/\* sqlite-large-value-flow: allow-for-now (?P<id>flow-h[0-9a-f]{12}) "
    r"owner=(?P<owner>[a-zA-Z0-9_:]+) TODO=(?P<todo>[a-z-]+) \*/"
)


def flow_site(path: str, source: str, function: str, ts: list[Token] | None = None) -> dict:
    if ts is None:
        ts = [t for t in tokens(source) if t.kind != "comment"]
        excluded = test_ranges(ts)
        ts = [t for t in ts if not any(a <= t.start < b for a, b in excluded)]
    matches = [i for i in range(len(ts) - 1) if ts[i].text == "fn" and ts[i + 1].text == function]
    if len(matches) != 1:
        raise ValueError(f"{path}: flow {function} must name one non-cfg(test) function, found {len(matches)}")
    start = matches[0]
    i = start + 2
    while i < len(ts):
        if ts[i].text in ("(", "["):
            i = paired(ts, i) + 1
        elif ts[i].text == "{":
            end = paired(ts, i)
            fingerprint = hashlib.sha256(" ".join(t.text for t in ts[start:end + 1]).encode()).hexdigest()
            return {"id": "flow-h" + hashlib.sha256(f"{path}\0{function}".encode()).hexdigest()[:12],
                    "path": path, "function": function, "owner": owner_for(path),
                    "fingerprint": fingerprint, "start": ts[start].start}
        elif ts[i].text == ";":
            break
        else:
            i += 1
    raise ValueError(f"{path}: flow {function} has no lexical body")


def inventory_delta(root: Path, manifest: dict) -> dict:
    """Propose source differences without writing files or accepting exceptions."""
    if manifest.get("version") != 1 or manifest.get("coverage") != "rust-source-sql-sites-v1":
        raise ValueError("unsupported inventory version/coverage")
    entries = manifest.get("sites")
    if not isinstance(entries, list) or any(not isinstance(e, dict) or
            not all(isinstance(e.get(k), str) for k in ("id", "path", "kind", "fingerprint", "owner", "todo"))
            for e in entries):
        raise ValueError("malformed old inventory; repair it before proposing changes")
    old = {e["id"]: e for e in entries}
    if len(old) != len(entries):
        raise ValueError("duplicate old inventory ID; proposal would be ambiguous")
    cache = {}
    sites, markers = scan(root, cache)
    current = {s.id: record(s) for s in sites}
    new_ids = set(current) - set(old)
    removed_ids = set(old) - set(current)
    changed = []
    by_path = {}
    for marker in markers:
        by_path.setdefault(marker[0], []).append(marker)
    ends = {path: [m[2] for m in ms] for path, ms in by_path.items()}
    counts = Counter(m[3]["id"] for m in markers if m[3])
    # A stale adjacent marker is an explicit identity anchor. Without one, do not
    # guess from line numbers, similar SQL text, or an arbitrary owner/TODO match.
    for site in sites:
        if site.id not in new_ids:
            continue
        index = bisect_right(ends.get(site.path, []), site.start) - 1
        marker = by_path[site.path][index] if index >= 0 else None
        if not marker or not marker[3] or cache[site.path][0][marker[2]:site.start].strip():
            continue
        prior_id = marker[3]["id"]
        if prior_id not in removed_ids or counts[prior_id] != 1:
            continue
        prior = old[prior_id]
        if prior["path"] != site.path or prior["kind"] != site.kind:
            continue
        changed.append({"anchor": "existing-adjacent-marker", "before": prior, "proposed": current[site.id]})
        new_ids.remove(site.id)
        removed_ids.remove(prior_id)
    for identifier in sorted(set(old) & set(current)):
        if any(old[identifier].get(k) != v for k, v in current[identifier].items()
               if k not in ("line_hint", "excerpt")):
            changed.append({"anchor": "same-id-metadata", "before": old[identifier], "proposed": current[identifier]})
    actual_declarations = declarations(root, sites, {p: v[0] for p, v in cache.items()})
    encode = lambda e: json.dumps(e, sort_keys=True, ensure_ascii=False)
    before = Counter(map(encode, manifest.get("declarations", [])))
    after = Counter(map(encode, actual_declarations))
    flow_changes = []
    active_tokens = {}
    for flow in manifest.get("flows", []):
        try:
            source, raw_tokens = cache[flow["path"]]
            if flow["path"] not in active_tokens:
                ts = [t for t in raw_tokens if t.kind != "comment"]
                excluded = test_ranges(ts)
                active_tokens[flow["path"]] = [t for t in ts if not any(a <= t.start < b for a, b in excluded)]
            actual = flow_site(flow["path"], source, flow["function"], active_tokens[flow["path"]])
            if (any(flow.get(k) != actual[k] for k in ("id", "owner", "fingerprint"))
                    or flow.get("disposition") != "allow-for-now"):
                proposed = {**flow, **{k: v for k, v in actual.items() if k != "start"},
                            "disposition": "allow-for-now"}
                flow_changes.append({"before": flow, "proposed": proposed})
        except (KeyError, ValueError) as error:
            flow_changes.append({"before": flow, "proposed": None, "source_error": str(error)})
    return {
        "mode": "proposal-only",
        "requires_human_owner_TODO_review": True,
        "writes_files": False,
        "notice": "Candidates are not accepted exceptions. Review producer, owner, TODO, marker and declaration changes; then run the guard. Unanchored replacements remain added plus removed. No size proof.",
        "added": [current[i] for i in sorted(new_ids, key=lambda i: (current[i]["path"], i))],
        "changed": sorted(changed, key=lambda e: (e["before"]["path"], e["before"]["id"])),
        "removed": [old[i] for i in sorted(removed_ids, key=lambda i: (old[i]["path"], i))],
        "declarations_added": [json.loads(e) for e, count in sorted((after - before).items()) for _ in range(count)],
        "declarations_removed": [json.loads(e) for e, count in sorted((before - after).items()) for _ in range(count)],
        "manual_flow_changes": sorted(flow_changes, key=lambda e: (e["before"]["path"], e["before"]["function"])),
        "sqlite_crates_before": manifest.get("sqlite_crates", []),
        "sqlite_crates_proposed": sqlite_crates(root),
    }
