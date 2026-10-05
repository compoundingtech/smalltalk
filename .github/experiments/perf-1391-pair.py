#!/usr/bin/env python3
"""Compare unchanged base and authored head on one runner with frozen inputs/gates."""
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

WORK = Path(os.environ['GITHUB_WORKSPACE'])
TEMP = Path(os.environ['RUNNER_TEMP'])
ROOT = TEMP / '1391-pair'
BASE = 'f61d4ddd7bf0ffad27b0c900aa700acd9dc25eaa'
HEAD = '6b21a5be4e2f64162e999280b5fc855533f7e7e4'
ALLOWED = {BASE, HEAD}
loader = importlib.machinery.SourceFileLoader('cache', str(TEMP / 'ci-perf-cache'))
spec = importlib.util.spec_from_loader(loader.name, loader)
cache = importlib.util.module_from_spec(spec)
loader.exec_module(cache)

def git(*args):
    return cache.git(WORK, *args)

def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def telemetry():
    result = {'utc': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()), 'cpu_count': os.cpu_count()}
    for name in ['/proc/loadavg', '/proc/pressure/cpu', '/proc/pressure/io', '/sys/fs/cgroup/cpu.max', '/sys/fs/cgroup/cpu.stat']:
        if Path(name).exists():
            result[name] = Path(name).read_text()
    return result

def initialize():
    ROOT.mkdir()
    (ROOT / 'results').mkdir()
    paths = ['crates/st3/tests/daemon_load.rs', 'crates/st3/tests/daemon_bench.rs', 'crates/st3/tests/perf_load.rs', 'scripts/ci-perf', 'scripts/ci-perf-cache', 'docs/st3/schema.md', 'Cargo.lock', 'flake.lock', 'flake.nix', 'Cargo.toml', 'crates/st3/Cargo.toml', '.cargo/config.toml']
    assert all(git('rev-parse', BASE + ':' + path) == git('rev-parse', HEAD + ':' + path) for path in paths)
    pr = cache.api('repos/' + os.environ['GITHUB_REPOSITORY'] + '/pulls/1391')
    assert pr['head']['sha'] == HEAD and pr['head']['repo']['full_name'] == os.environ['GITHUB_REPOSITORY']
    baseline = TEMP / 'perf-baseline'
    assert len(list(baseline.glob('load-*.json'))) == 5
    for path in baseline.glob('load-*.json'):
        assert cache.valid_report(json.loads(path.read_text()))
    shutil.copytree(baseline, ROOT / 'baselines')
    shutil.copytree(TEMP / 'st-bench', ROOT / 'stores')
    assert (ROOT / 'stores/generated-1.sqlite3').is_file()
    assert (ROOT / 'stores/generated-1-peer.sqlite3').is_file()
    shutil.move(WORK / 'target', ROOT / 'seed-target')
    receipt = {'base': BASE, 'head': HEAD, 'scope': 'Same-runner exact-source diagnostic. Original gates and failures retained; no deployment/idle acceptance.', 'baseline_sha256': {p.name: digest(p) for p in baseline.glob('load-*.json')}, 'store_sha256': {p.name: digest(p) for p in (ROOT / 'stores').glob('generated-1*.sqlite3*')}, 'hardware': telemetry(), 'cpu': subprocess.check_output(['lscpu'], text=True)}
    (ROOT / 'provenance.json').write_text(json.dumps(receipt, indent=2))

def measure(label, head):
    assert head in ALLOWED and not git('status', '--porcelain')
    subprocess.run(['git', 'checkout', '--detach', head], cwd=WORK, check=True)
    assert git('rev-parse', 'HEAD') == head and not git('status', '--porcelain')
    saved = ROOT / 'compiled' / head
    saved.mkdir(parents=True, exist_ok=True)
    assert not (WORK / 'target').exists()
    if (saved / 'target').exists():
        shutil.move(saved / 'target', WORK / 'target')
        checkpoint = saved / 'source-state.json'
        state = json.loads(checkpoint.read_text())
        assert state['head'] == head
        cache.restore_sources(checkpoint)
        build_origin = 'same-runner prior exact-source build'
    else:
        subprocess.run(['cp', '-a', str(ROOT / 'seed-target'), str(WORK / 'target')], check=True)
        build_origin = 'same restored compatible main cache, Cargo validates exact source'
    shutil.rmtree(TEMP / 'st-bench', ignore_errors=True)
    subprocess.run(['cp', '-a', str(ROOT / 'stores'), str(TEMP / 'st-bench')], check=True)
    shutil.rmtree(TEMP / 'perf', ignore_errors=True)
    expected = json.loads((ROOT / 'provenance.json').read_text())
    assert {p.name: digest(p) for p in (TEMP / 'perf-baseline').glob('load-*.json')} == expected['baseline_sha256']
    inputs = {p.name: digest(p) for p in (TEMP / 'st-bench').glob('generated-1*.sqlite3*')}
    assert inputs == expected['store_sha256']
    out = ROOT / 'results' / label
    out.mkdir()
    before = telemetry()
    started = time.monotonic()
    subprocess.run(['bash', 'scripts/ci-nix-cache', 'use'], cwd=WORK, check=True)
    verdict = subprocess.run(['nix', 'develop', '.#perf', '-c', 'bash', 'scripts/ci-perf', 'load'], cwd=WORK).returncode
    shutil.copytree(TEMP / 'perf', out / 'perf')
    report = json.loads((out / 'perf/load.json').read_text())
    assert cache.valid_report(report)
    assert report['claims'] == 238794
    receipt = {'label': label, 'head': head, 'exit_status': verdict, 'elapsed_seconds': time.monotonic() - started, 'build_origin': build_origin, 'input_store_sha256': inputs, 'baseline_sha256': expected['baseline_sha256'], 'telemetry_before': before, 'telemetry_after': telemetry(), 'source_clean_after': not bool(git('status', '--porcelain'))}
    (out / 'receipt.json').write_text(json.dumps(receipt, indent=2))
    assert receipt['source_clean_after']
    shutil.move(WORK / 'target', saved / 'target')
    shutil.copyfile(TEMP / 'perf-source-state.json', saved / 'source-state.json')
    print(json.dumps(receipt), flush=True)

if sys.argv[1] == 'init':
    initialize()
elif sys.argv[1] == 'measure':
    measure(sys.argv[2], sys.argv[3])
elif sys.argv[1] == 'verdict':
    receipts = [json.loads(p.read_text()) for p in sorted((ROOT / 'results').glob('*/receipt.json'))]
    assert len(receipts) == 4
    sys.exit(int(any(receipt['exit_status'] for receipt in receipts)))
else:
    raise ValueError('unknown stage')
