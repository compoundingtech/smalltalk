"""Production module graph of the st3 source tree, and the layering rule checked against it.

A module is a top-level file or directory under a source directory of st3 (crates/st3/src, and the
crates that st3's library is split into). A reference from module A to module B is a path in
non-test code that starts `crate::B` or `st3::B`, or a name in a `crate::{...}` group. Comments,
`#[cfg(test)]` items, `*_tests.rs`, `tests/` and `test_support.rs` are left out. Re-exported items
that the library names at its root count toward the module that defines them (REEXPORTS).

The rule: the graph minus the allowlisted pairs in .st/module-layers.json is acyclic. A new edge
that closes a cycle fails the check; an allowlisted pair that no longer exists must be removed, so
the list only shrinks. Layers are computed (longest path above the leaves), not assigned by hand.
"""
import collections
import json
import os
import re
import tomllib

SOURCE_DIRS = ("crates/st3-core/src", "crates/st3-kernel/src", "crates/st3-telemetry/src", "crates/st3-daemon/src", "crates/st3/src")
ENTRY_FILES = {"lib", "main", "fixture"}
TEST_ONLY_MODULES = {"test_support"}
REEXPORTS = {
    "NormalizedIntent": "model",
    "St3Error": "model",
    "cli_print": "cli_output",
    "cli_println": "cli_output",
    "parse_intent": "graph",
    "validate_mission_runtimes": "graph",
}
LAYERS_FILE = ".st/module-layers.json"
_TEST_PATH = re.compile(r"(_tests?\.rs$|/tests?\.rs$|/tests/)")
_CFG_TEST = re.compile(
    r"#\[cfg\(\s*(?:test|all\(test[^)]*\))\s*\)\]\s*(?:#\[[^\]]*\]\s*)*"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:mod\s+\w+\s*\{|fn\s|use\s|impl|const|static|struct|enum|mod\s+\w+;|async fn|type|trait)"
)


def strip_comments(source):
    """Remove line and (nested) block comments, leaving string and char literals intact."""
    out, cursor, start = [], 0, 0
    while cursor < len(source):
        skipped = _skip_literal(source, cursor)
        if skipped != cursor:
            cursor = skipped
            continue
        if source.startswith("//", cursor):
            out.append(source[start:cursor])
            end = source.find("\n", cursor)
            cursor = start = len(source) if end < 0 else end
            continue
        if source.startswith("/*", cursor):
            out.append(source[start:cursor])
            depth, cursor = 1, cursor + 2
            while cursor < len(source) and depth:
                if source.startswith("/*", cursor):
                    depth, cursor = depth + 1, cursor + 2
                elif source.startswith("*/", cursor):
                    depth, cursor = depth - 1, cursor + 2
                else:
                    cursor += 1
            start = cursor
            continue
        cursor += 1
    out.append(source[start:])
    return "".join(out)


def drop_cfg_test(source):
    out, index = [], 0
    while True:
        match = _CFG_TEST.search(source, index)
        if not match:
            out.append(source[index:])
            return "".join(out)
        out.append(source[index:match.start()])
        stop = re.search(r"[{;]", source[match.end() - 1:])
        position = match.end() - 1 + stop.start()
        if source[position] == ";":
            index = position + 1
            continue
        depth, cursor = 0, position
        while cursor < len(source):
            skipped = _skip_literal(source, cursor)
            if skipped != cursor:
                cursor = skipped
                continue
            if source[cursor] == "{":
                depth += 1
            elif source[cursor] == "}":
                depth -= 1
                if depth == 0:
                    break
            cursor += 1
        index = cursor + 1


_RAW_STRING = re.compile(r'b?r(#*)"')
_CHAR = re.compile(r"b?'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}|.)|[^\\'\n])'")


def _skip_literal(source, cursor):
    """The index after a string, raw string or char literal starting at `cursor`, else `cursor`.
    Braces inside literals (KDL and JSON in test fixtures) must not close the item being skipped."""
    raw = _RAW_STRING.match(source, cursor)
    if raw and (cursor == 0 or not (source[cursor - 1].isalnum() or source[cursor - 1] == "_")):
        end = source.find('"' + raw.group(1), raw.end())
        return len(source) if end < 0 else end + 1 + len(raw.group(1))
    char = source[cursor]
    if char == '"' or (char == "b" and source[cursor + 1:cursor + 2] == '"'):
        index = cursor + (2 if char == "b" else 1)
        while index < len(source):
            if source[index] == "\\":
                index += 2
                continue
            if source[index] == '"':
                return index + 1
            index += 1
        return len(source)
    literal = _CHAR.match(source, cursor)
    if literal and (cursor == 0 or not (source[cursor - 1].isalnum() or source[cursor - 1] == "_")):
        return literal.end()
    return cursor


def path_targets(source):
    counts = collections.Counter()
    for match in re.finditer(r"\b(?:crate|st3)::", source):
        start = match.end()
        if source[start:start + 1] == "{":
            depth, cursor, begin, items = 0, start + 1, start + 1, []
            while cursor < len(source):
                char = source[cursor]
                if char == "{":
                    depth += 1
                elif char == "}":
                    if depth == 0:
                        items.append(source[begin:cursor])
                        break
                    depth -= 1
                elif char == "," and depth == 0:
                    items.append(source[begin:cursor])
                    begin = cursor + 1
                cursor += 1
            for item in items:
                name = re.match(r"\s*(\w+)", item)
                if name:
                    counts[name.group(1)] += 1
        else:
            name = re.match(r"(\w+)", source[start:])
            if name:
                counts[name.group(1)] += 1
    return counts


def source_dirs(root):
    return [os.path.join(root, d) for d in SOURCE_DIRS if os.path.isdir(os.path.join(root, d))]


def module_of(directory, path):
    top = os.path.relpath(path, directory).split(os.sep)[0]
    return top[:-3] if top.endswith(".rs") else top


def production_graph(root):
    """Return (modules, edges) where edges maps module -> {module: reference count}."""
    files = collections.defaultdict(list)
    for directory in source_dirs(root):
        for base, _, names in os.walk(directory):
            for name in names:
                path = os.path.join(base, name)
                if not name.endswith(".rs") or _TEST_PATH.search(path.replace(os.sep, "/")):
                    continue
                module = module_of(directory, path)
                if module in ENTRY_FILES or module in TEST_ONLY_MODULES or module.endswith("_tests"):
                    continue
                files[module].append(path)
    modules = set(files)
    edges = {module: collections.Counter() for module in modules}
    for module, paths in files.items():
        for path in paths:
            with open(path, errors="replace") as handle:
                source = drop_cfg_test(strip_comments(handle.read()))
            for target, count in path_targets(source).items():
                target = REEXPORTS.get(target, target)
                if target in modules and target != module:
                    edges[module][target] += count
    return modules, {module: dict(targets) for module, targets in edges.items()}


def load_layers(root):
    with open(os.path.join(root, LAYERS_FILE)) as handle:
        data = json.load(handle)
    if data.get("version") != 1:
        raise ValueError(f"{LAYERS_FILE}: unsupported version")
    return {tuple(pair) for pair in data["allowed_upward"]}


def without(edges, allowed):
    return {a: {b: n for b, n in targets.items() if (a, b) not in allowed} for a, targets in edges.items()}


def find_cycle(edges):
    """One cycle as a list of modules, or None."""
    state, stack = {}, []

    def visit(module):
        state[module] = 1
        stack.append(module)
        for target in sorted(edges.get(module, ())):
            if state.get(target) == 1:
                return stack[stack.index(target):] + [target]
            if target not in state:
                found = visit(target)
                if found:
                    return found
        stack.pop()
        state[module] = 2
        return None

    for module in sorted(edges):
        if module not in state:
            found = visit(module)
            if found:
                return found
    return None


def layers(edges):
    """Longest path above the leaves; the edges must be acyclic."""
    depth = {}

    def level(module):
        if module not in depth:
            depth[module] = 0
            depth[module] = 1 + max((level(t) for t in edges.get(module, ())), default=-1)
        return depth[module]

    for module in edges:
        level(module)
    return depth


BINARY_SIDE_CRATES = ("stui", "st3-conversation-ui")


def client_alias_findings(manifest_path, workspace_dependencies):
    """Reject renamed client dependencies after resolving workspace inheritance."""
    if not os.path.isfile(manifest_path):
        return []
    with open(manifest_path, "rb") as handle:
        manifest = tomllib.load(handle)
    problems = []

    def inspect(table, location=()):
        for key, value in table.items():
            if not isinstance(value, dict):
                continue
            if key in ("dependencies", "dev-dependencies", "build-dependencies"):
                for name, spec in value.items():
                    resolved = workspace_dependencies.get(name, {}) \
                        if isinstance(spec, dict) and spec.get("workspace") is True else spec
                    package = resolved.get("package", name) if isinstance(resolved, dict) else name
                    if name != "st3-client" and package == "st3-client":
                        section = ".".join((*location, key))
                        problems.append(
                            f"{manifest_path}: [{section}] aliases st3-client as {name}; "
                            "the client dependency must retain its canonical name"
                        )
            else:
                inspect(value, (*location, key))

    inspect(manifest)
    return problems


def client_boundary_findings(root):
    """The kernel and telemetry may not reach the standalone client's API."""
    problems = []
    workspace_dependencies = {}
    workspace_manifest = os.path.join(root, "Cargo.toml")
    if os.path.isfile(workspace_manifest):
        with open(workspace_manifest, "rb") as handle:
            workspace_dependencies = tomllib.load(handle).get("workspace", {}).get("dependencies", {})
    for crate in ("st3-kernel", "st3-telemetry"):
        crate_directory = os.path.join(root, "crates", crate)
        problems.extend(client_alias_findings(
            os.path.join(crate_directory, "Cargo.toml"), workspace_dependencies
        ))
        for target in ("src", "tests", "benches", "examples"):
            for base, _, names in os.walk(os.path.join(crate_directory, target)):
                for name in sorted(names):
                    if not name.endswith(".rs"):
                        continue
                    path = os.path.join(base, name)
                    with open(path, errors="replace") as handle:
                        source = strip_comments(handle.read())
                    # Literals (including source snippets in tests) do not name a crate.
                    code, cursor = [], 0
                    while cursor < len(source):
                        end = _skip_literal(source, cursor)
                        if end != cursor:
                            code.append("".join("\n" if c == "\n" else " " for c in source[cursor:end]))
                            cursor = end
                        else:
                            code.append(source[cursor])
                            cursor += 1
                    source = "".join(code)
                    for match in re.finditer(r"\bst3_client\b", source):
                        allowed = False
                        # LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-16-state-dirs-fractal — DELETE at contraction — #3032
                        # The standalone client owns the single R11 directory resolver.
                        allowed = re.match(r"\s*::\s*product_dirs\b", source[match.end():]) is not None
                        # LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-16-state-dirs-fractal
                        if not allowed:
                            line = source.count("\n", 0, match.start()) + 1
                            problems.append(
                                f"{os.path.relpath(path, root)}:{line}: forbidden st3_client reference"
                            )
    return problems


def crate_findings(root):
    """Facts about the st3 and st3-daemon packages that docs/st3/crate-contract.md promises.

    They apply once crates/st3-daemon exists: the library does not depend on a binary-side crate,
    the two packages carry the same version (the library reports CARGO_PKG_VERSION), a dependency both
    manifests name is declared the same, and their build.rs files are the same (both read the
    ST3_FIXTURE_* variables for the same feature).
    """
    daemon = os.path.join(root, "crates", "st3-daemon")
    if not os.path.isfile(os.path.join(daemon, "Cargo.toml")):
        return []
    problems = []

    def manifest(directory):
        with open(os.path.join(directory, "Cargo.toml"), "rb") as handle:
            return tomllib.load(handle)

    library, binary = manifest(daemon), manifest(os.path.join(root, "crates", "st3"))
    if library["package"]["version"] != binary["package"]["version"]:
        problems.append(f"crates/st3-daemon version {library['package']['version']} differs from "
                        f"crates/st3 version {binary['package']['version']}")
    for section in ("dependencies", "build-dependencies"):
        for key, spec in library.get(section, {}).items():
            name = spec.get("package", key) if isinstance(spec, dict) else key
            if name in BINARY_SIDE_CRATES:
                problems.append(f"crates/st3-daemon depends on {name}, which is binary-side")
    # Both manifests list the library's dependencies, so a bump in one that the other misses would put
    # two versions of a crate into the build: a dependency named in both must be declared the same.
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        for key, spec in sorted(library.get(section, {}).items()):
            other = binary.get(section, {}).get(key)
            if other is not None and other != spec:
                problems.append(f"{key} in [{section}] differs between crates/st3-daemon ({spec}) and crates/st3 ({other})")
    # A nextest filter that names package st3 and not st3-daemon silently stops matching the library's
    # tests once they live in st3-daemon; only filters for the binary-side integration tests may do so.
    nextest = os.path.join(root, ".config", "nextest.toml")
    if os.path.isfile(nextest):
        with open(nextest, "rb") as handle:
            config = tomllib.load(handle)
        for profile, body in config.get("profile", {}).items():
            filters = [(f"profile.{profile}.default-filter", body["default-filter"])] if "default-filter" in body else []
            for kind in ("overrides", "scripts"):
                for index, entry in enumerate(body.get(kind, [])):
                    if "filter" in entry:
                        filters.append((f"profile.{profile}.{kind}[{index}]", entry["filter"]))
            for where, expression in filters:
                if re.search(r"package\(=?st3\)", expression) and "st3-daemon" not in expression \
                        and "binary(=integration)" not in expression:
                    problems.append(f".config/nextest.toml {where} names package st3 but not st3-daemon, "
                                    "so it no longer selects the library's tests")
    build = [os.path.join(d, "build.rs") for d in (daemon, os.path.join(root, "crates", "st3"))]
    contents = []
    for path in build:
        try:
            with open(path) as handle:
                contents.append(handle.read())
        except FileNotFoundError:
            contents.append(None)
    if contents[0] != contents[1]:
        problems.append("crates/st3-daemon/build.rs and crates/st3/build.rs differ or one is missing")
    return problems


def findings(root):
    modules, edges = production_graph(root)
    allowed = load_layers(root)
    present = {(a, b) for a, targets in edges.items() for b in targets}
    problems = []
    for pair in sorted(allowed - present):
        problems.append(f"{LAYERS_FILE}: allowed upward pair {pair[0]} -> {pair[1]} no longer exists; remove it")
    for pair in sorted(allowed):
        if pair[0] not in modules or pair[1] not in modules:
            problems.append(f"{LAYERS_FILE}: {pair[0]} -> {pair[1]} names a module that does not exist")
    cycle = find_cycle(without(edges, allowed & present))
    if cycle:
        problems.append("module cycle outside the allowlist: " + " -> ".join(cycle))
    return problems + crate_findings(root) + client_boundary_findings(root)


def reverse_closure(edges, start):
    reverse = collections.defaultdict(set)
    for module, targets in edges.items():
        for target in targets:
            reverse[target].add(module)
    seen, pending = set(start), list(start)
    while pending:
        for module in reverse[pending.pop()]:
            if module not in seen:
                seen.add(module)
                pending.append(module)
    return seen


def modules_of_paths(root, paths):
    """Modules named by changed paths, and the paths that belong to no st3 module."""
    found, other = set(), []
    for path in paths:
        for directory in SOURCE_DIRS:
            prefix = directory + "/"
            if path.startswith(prefix):
                top = path[len(prefix):].split("/")[0]
                found.add(top[:-3] if top.endswith(".rs") else top)
                break
        else:
            other.append(path)
    return found, other
