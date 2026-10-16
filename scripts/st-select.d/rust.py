"""Rust plugin for scripts/st-select: what the Rust test stage of linux-gate reads, and what it never reads.

INPUTS are the paths whose content can change a Rust build or a Rust test result (compile-time includes, build
scripts, files tests open or spawn). When the base policy declares `inputs` for linux-gate those win; this list is the
fallback and must stay a superset of it (st-select-test checks that). NOT_MINE is the reviewed list of paths no Rust
build or test reads; a changed path in neither list is unknown, and unknown runs everything.

ALWAYS are the tests that read every file of the tree (forbidden words, the public-repository scan, the vrs ledger):
no input list can cover them, so they run on every tree and the record says they ran.
"""

CHECK = 'linux-gate'

INPUTS = (
    '.cargo', '.config/nextest.toml', '.github', 'Cargo.lock', 'Cargo.toml', 'claude-channel', 'clients', 'components',
    'crates', 'docs/st2', 'docs/st3', 'docs/vrs', 'evals', 'examples', 'fixtures', 'flake.lock', 'flake.nix', 'nix',
    'plugins', 'scripts', 'slo', 'src', 'templates', 'tests', 'vendor', 'wit',
    # crates/st3/src/attach_image.rs:422 reads it at run time.
    'apps/fractal-web/src/terminal/assets/JetBrainsMonoNerdFontMono-Regular.ttf',
)

NOT_MINE = (
    'release-notes', 'apps', 'packages', 'buck2', 'docs', 'AGENTS.md', 'CLAUDE.md', 'README.md', 'README.st2.md',
    'BUCK', 'BUCK.genie.ts', '.buckconfig', '.buckconfig.genie.ts', '.buckroot', 'package.json', 'package.json.genie.ts',
    'pnpm-lock.yaml', 'pnpm-workspace.yaml', 'pnpm-workspace.yaml.genie.ts', 'install.sh', '.st',
)

# tests/integration.rs mounts these three; each reads the whole tree (tests/public_repo.rs, tests/native_only.rs:395,
# tests/vrs_ledger.rs:111). The filter is exact so nothing else in the binary runs.
ALWAYS = {
    'whole-tree-lints': ['cargo', 'nextest', 'run', '--no-fail-fast', '-p', 'st2', '--test', 'integration',
                         '-E', 'test(/^(public_repo|native_only|vrs_ledger)::/)'],
}

# Crates whose dev-dependency cycles make them one test component (cargo metadata, 2026-10-16).
CYCLE = {'st3', 'st3-client', 'stui', 'st3-feed', 'st3-conversation-ui', 'st3-ui-model', 'st3-client-tui', 'st3-migrate'}


def report(metadata, changed):
    """For a changed Rust tree: the crates whose tests a change reaches and the nextest filter that names them.

    A report only. Running fewer tests on a changed Rust tree is tiering, which the plan rejects; st-select never
    runs this filter, it prints it so the replay can say how much it would have narrowed."""
    import os
    root = metadata['workspace_root']
    members = set(metadata['workspace_members'])
    packages = {p['id']: p for p in metadata['packages'] if p['id'] in members}
    dirs = sorted(((os.path.relpath(os.path.dirname(p['manifest_path']), root), p['name']) for p in packages.values()),
                  key=lambda item: -len(item[0]))
    reverse = {p['name']: set() for p in packages.values()}
    for node in metadata['resolve']['nodes']:
        if node['id'] not in members:
            continue
        for dep in node['deps']:
            if dep['pkg'] in members:
                reverse[packages[dep['pkg']]['name']].add(packages[node['id']]['name'])
    touched, everything = set(), False
    for path in changed:
        owner = next((name for d, name in dirs if d != '.' and (path == d or path.startswith(d + '/'))), None)
        if owner is None and (path.startswith(('src/', 'tests/')) or path == 'build.rs'):
            owner = 'st2'
        if owner is not None:
            touched.add(owner)
        elif any(path == p or path.startswith(p + '/') for p in INPUTS):
            everything = True  # a shared input (Cargo.lock, docs/st3, scripts, ...) reaches every crate
    if everything:
        return {'crates': 'all', 'filter': 'all()'}
    reached, stack = set(touched), list(touched)
    while stack:
        for user in reverse.get(stack.pop(), ()):
            if user not in reached:
                reached.add(user)
                stack.append(user)
    names = sorted(reached)
    return {'crates': names, 'filter': ' | '.join(f'package(={n})' for n in names) or 'none()'}
