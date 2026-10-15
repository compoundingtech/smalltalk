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

SOURCE_DIRS = ("crates/st3-core/src", "crates/st3-daemon/src", "crates/st3/src")
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
    source = re.sub(r"/\*.*?\*/", "", source, flags=re.S)
    return re.sub(r"//[^\n]*", "", source)


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
            if source[cursor] == "{":
                depth += 1
            elif source[cursor] == "}":
                depth -= 1
                if depth == 0:
                    break
            cursor += 1
        index = cursor + 1


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


def crate_findings(root):
    """Facts about the st3 and st3-daemon packages that docs/st3/crate-contract.md promises.

    They apply once crates/st3-daemon exists: the library does not depend on a binary-side crate,
    the two packages carry the same version (the library reports CARGO_PKG_VERSION), and their
    build.rs files are the same (both read the ST3_FIXTURE_* variables for the same feature).
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
    for name in BINARY_SIDE_CRATES:
        if name in library.get("dependencies", {}):
            problems.append(f"crates/st3-daemon depends on {name}, which is binary-side")
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
    return problems + crate_findings(root)


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
