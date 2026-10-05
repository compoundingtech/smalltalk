#!/usr/bin/env python3
"""Compare pinned main, old head and optimized head on one Namespace worker; unchanged gates."""
import hashlib
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor

WORK = Path(os.environ['GITHUB_WORKSPACE'])
TEMP = Path(os.environ['RUNNER_TEMP'])
ROOT = TEMP / '1322-optimized'
REPO = os.environ['GITHUB_REPOSITORY']
GOOD = '792d6d3eb98333115858d014b6efde982d9285d4'
BAD = '5b58f1d11c84451ad2d3128cdaffa84d91f0e054'
FIXED = '710283b51b96d8c514f24b31c6c7addd9735e492'
AFTERNOON = '2e075af9ef8c683d55958283b759e3466154ce10'
LATEST = '5f8413ae1a5ea4d0965207a4a1f852940f3650c1'
BASELINES = [37237521042, 37236466976, 37236198934, 37222992444, 37219728448]
SEEDS = [(GOOD, 37239143830), (BAD, 37243910216), (FIXED, 37252089845)]
ALLOWED = {LATEST, BAD, FIXED}
BUILD = ['Cargo.lock', 'flake.lock', 'flake.nix', 'Cargo.toml', 'crates/st3/Cargo.toml', '.cargo/config.toml']
STORES = ['crates/st3/tests/daemon_bench.rs', 'docs/st3/schema.md']
loader = importlib.machinery.SourceFileLoader('cache', str(TEMP / 'ci-perf-cache'))
spec = importlib.util.spec_from_loader(loader.name, loader)
cache = importlib.util.module_from_spec(spec)
loader.exec_module(cache)

def git(*args):
    return cache.git(WORK, *args)

def signature(head, paths):
    return [git('rev-parse', f'{head}:{path}') for path in paths]

def seed_for(head, paths):
    return next(sha for sha, _ in SEEDS if signature(head, paths) == signature(sha, paths))

def telemetry():
    data = {'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), 'cpu_count': os.cpu_count()}
    for name in ['/proc/loadavg', '/proc/pressure/cpu', '/proc/pressure/io', '/sys/fs/cgroup/cpu.max', '/sys/fs/cgroup/cpu.stat']:
        if Path(name).exists():
            data[name] = Path(name).read_text()
    return data

def restore_checkpoint(source):
    # The main helper reads its checkpoint at the established cache path.
    destination = TEMP / 'st-ci-cache/perf-source-state.json'
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    cache.restore_sources()

def initialize():
    ROOT.mkdir()
    (ROOT / 'results').mkdir()
    (ROOT / 'baselines').mkdir()
    subprocess.run(['git', 'merge-base', '--is-ancestor', AFTERNOON, LATEST], cwd=WORK, check=True)
    subprocess.run(['git', 'merge-base', '--is-ancestor', AFTERNOON, BAD], cwd=WORK, check=True)
    pr = cache.api(f'repos/{REPO}/pulls/1322')
    assert pr['head']['sha'] == FIXED and pr['head']['repo']['full_name'] == REPO
    assert len({git('rev-parse', h + ':crates/st3/tests/daemon_load.rs') for h in ALLOWED}) == 1
    assert len({git('rev-parse', h + ':crates/st3/tests/daemon_bench.rs') for h in ALLOWED}) == 1
    subprocess.run(['git', 'merge-base', '--is-ancestor', BAD, FIXED], cwd=WORK, check=True)
    assert signature(BAD, STORES) == signature(FIXED, STORES)
    assert len({tuple(signature(h, BUILD)) for h in ALLOWED | {GOOD}}) == 1
    hardware = telemetry()
    hardware['cpu'] = subprocess.check_output(['lscpu'], text=True)
    (ROOT / 'hardware.json').write_text(json.dumps(hardware, indent=2))
    def seed(item):
        sha, run_id = item
        run = cache.api(f'repos/{REPO}/actions/runs/{run_id}')
        assert run['head_sha'] == sha
        if sha in (BAD, FIXED):
            assert (run['event'] == 'pull_request' and run['path'] == '.github/workflows/perf.yml'
                    and run['head_branch'] == 'agent/harness-needs-login'
                    and run['head_repository']['full_name'] == REPO
                    and run['conclusion'] in ('success', 'failure'))
        else:
            assert cache.trusted_snapshot(run)
        artifacts = cache.api(f'repos/{REPO}/actions/runs/{run_id}/artifacts?per_page=100')['artifacts']
        dest = ROOT / 'seeds' / sha
        dest.mkdir(parents=True)
        kinds = [('build', ('target', 'cargo-home', 'perf-sccache', 'st-ci-cache'))]
        if sha == GOOD:
            kinds.append(('stores', ('st-bench',)))
        for kind, roots in kinds:
            names = [a['name'] for a in artifacts if a['name'].startswith(f'perf-load-{kind}-v1-') and not a['expired']]
            assert len(names) == 1, (run_id, kind, names)
            archive = dest / kind
            cache.download(REPO, run, names[0], archive)
            cache.unpack(archive / f'{kind}.tar.zst', dest, roots)
            (archive / f'{kind}.tar.zst').unlink()
        store_hashes = {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                        for path in (dest / 'st-bench').glob('generated-1*.sqlite3*')}
        if sha == GOOD:
            assert 'generated-1.sqlite3' in store_hashes and 'generated-1-peer.sqlite3' in store_hashes
        return {'head': sha, 'run': run_id, 'store_sha256': store_hashes,
                'artifacts': [a['name'] for a in artifacts if not a['expired']]}
    def baseline(run_id):
        run = cache.api(f'repos/{REPO}/actions/runs/{run_id}')
        assert cache.trusted(run)
        dest = ROOT / 'baseline-download' / str(run_id)
        cache.download(REPO, run, 'perf-load-logs', dest)
        files = list(dest.rglob('load.json'))
        assert len(files) == 1 and cache.valid_report(json.loads(files[0].read_text()))
        target = ROOT / 'baselines' / f'load-{run_id}.json'
        shutil.copyfile(files[0], target)
        return {'run': run_id, 'head': run['head_sha'], 'sha256': hashlib.sha256(target.read_bytes()).hexdigest()}
    with ThreadPoolExecutor(max_workers=3) as pool:
        seeds = list(pool.map(seed, SEEDS))
        baselines = list(pool.map(baseline, BASELINES))
    (ROOT / 'provenance.json').write_text(json.dumps({'seeds': seeds, 'baselines': baselines}, indent=2))
    for name in ['cargo-home', 'perf-sccache']:
        shutil.rmtree(TEMP / name, ignore_errors=True)
        shutil.move(ROOT / 'seeds' / GOOD / name, TEMP / name)
    shutil.copytree(ROOT / 'baselines', TEMP / 'perf-baseline', dirs_exist_ok=True)
    print('Frozen five successful-main baselines; trusted snapshots downloaded.', flush=True)

def measure(label, requested):
    head = git('rev-parse', requested)
    assert head in ALLOWED
    assert not git('status', '--porcelain'), 'checkout must be clean'
    subprocess.run(['git', 'checkout', '--detach', head], cwd=WORK, check=True)
    assert git('rev-parse', 'HEAD') == head and not git('status', '--porcelain')
    build_seed = seed_for(head, BUILD)
    store_seed = GOOD
    # Intentional experiment input: identical immutable main corpus for all matched cases.
    # This is not a compatible-cache claim; native head corpus is measured separately.
    assert signature(head, ['crates/st3/tests/daemon_bench.rs']) == signature(store_seed, ['crates/st3/tests/daemon_bench.rs'])
    saved = ROOT / 'compiled' / head
    saved.mkdir(parents=True, exist_ok=True)
    if (saved / 'target').exists():
        shutil.move(saved / 'target', WORK / 'target')
        restore_checkpoint(saved / 'source-state.json')
        origin = 'same-runner exact-SHA previous measurement'
    else:
        source = ROOT / 'seeds' / build_seed
        # Use BAD's exact compiled snapshot for the BAD endpoint, not an older compatible one.
        if head in dict(SEEDS):
            source = ROOT / 'seeds' / head
        subprocess.run(['cp', '-a', str(source / 'target'), str(WORK / 'target')], check=True)
        restore_checkpoint(source / 'st-ci-cache/perf-source-state.json')
        origin = f'verified own-PR {source.name}' if source.name in (BAD, FIXED) else f'trusted main {source.name}'
    shutil.rmtree(TEMP / 'st-ci-cache', ignore_errors=True)
    shutil.copytree(ROOT / 'seeds' / build_seed / 'st-ci-cache', TEMP / 'st-ci-cache')
    shutil.rmtree(TEMP / 'st-bench', ignore_errors=True)
    subprocess.run(['cp', '-a', str(ROOT / 'seeds' / store_seed / 'st-bench'), str(TEMP / 'st-bench')], check=True)
    shutil.rmtree(TEMP / 'perf', ignore_errors=True)
    out = ROOT / 'results' / label
    out.mkdir()
    before = telemetry()
    started = time.monotonic()
    subprocess.run(['bash', 'scripts/ci-nix-cache', 'use'], cwd=WORK, check=True)
    status = subprocess.run(['nix', 'develop', '.#perf', '-c', 'bash', 'scripts/ci-perf', 'load'], cwd=WORK).returncode
    after = telemetry()
    shutil.copytree(TEMP / 'perf', out / 'perf')
    report = json.loads((out / 'perf/load.json').read_text())
    assert cache.valid_report(report), 'missing or incomplete workload report'
    result = dict(label=label, head=head, exit_status=status, seconds=time.monotonic()-started,
                  build_origin=origin, store_origin=store_seed, input_mode='matched-main',
                  replication_p99_ms=report['paths']['replication exchange']['p99_ms'],
                  replication_p50_ms=report['paths']['replication exchange']['p50_ms'], cpu=report['daemon_cores'],
                  telemetry_before=before, telemetry_after=after)
    (out / 'result.json').write_text(json.dumps(result, indent=2))
    shutil.move(WORK / 'target', saved / 'target')
    shutil.copyfile(TEMP / 'perf-source-state.json', saved / 'source-state.json')
    assert not git('status', '--porcelain')
    print(json.dumps({k: v for k,v in result.items() if not k.startswith('telemetry')}), flush=True)
    # Every original gate failure is recorded and propagated by the final verdict.

if sys.argv[1] == 'init':
    initialize()
elif sys.argv[1] == 'measure':
    measure(sys.argv[2], sys.argv[3])
elif sys.argv[1] == 'verdict':
    results = [json.loads(p.read_text()) for p in sorted((ROOT / 'results').glob('*/result.json'))]
    (ROOT / 'results.json').write_text(json.dumps(results, indent=2))
    sys.exit(int(any(r['exit_status'] for r in results)))
