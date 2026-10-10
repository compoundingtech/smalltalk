#!/usr/bin/env python3
"""Hosted-only three-source controller. Local imports and pure controls launch nothing."""
import argparse
import importlib.util
import math
import os
from pathlib import Path
import re
import signal
import sys
import time

spec = importlib.util.spec_from_file_location('diet', Path(__file__).with_name('ci-log-diet-common.py'))
c = importlib.util.module_from_spec(spec); spec.loader.exec_module(c)

BUNDLE = '24e647764a5ea4a295c0d20d54e0eec99770b40812352fb332bede83b1fb9a8b'
PATCH = '1f7f1c17318b6c2f7b4be79555b61bed37102bea81be2e2e4d3279aba7ee3ed2'
ASSIGNMENT = r'doc/fleet/smalltalk/speed/2026-10-10/log-diet-three-source-execution-assignment@[0-9a-f]{64}'


def verify_inputs(repo):
    data = repo / 'data/log-diet-three-source'
    if c.sha(data / 'source.bundle') != BUNDLE or c.sha(data / 'diagnostic.patch') != PATCH:
        raise ValueError('immutable transfer mismatch')
    manifest = c.load(data / 'automation-manifest.json')
    for row in manifest['files']:
        path = repo / row['path']
        if path.stat().st_size != row['bytes'] or c.sha(path) != row['sha256']:
            raise ValueError('automation input mismatch')
    plans = c.load(data / 'sources.json')
    if tuple(p['commit'] for p in plans) != c.SOURCES:
        raise ValueError('missing/extra/reordered source')
    return data, plans, manifest


def prepare_arm(plan, source, data, runner, active, end):
    # All source import/checkouts happen only in the later authorized hosted job.
    runner.checked('clone-' + plan['name'], ['git', 'clone', '--no-checkout', str(data / 'source.bundle'), str(source)],
                   data, active, end)
    runner.checked('checkout-' + plan['name'], ['git', 'checkout', '--detach', plan['commit']], source, active, end)
    runner.checked('tree-' + plan['name'], ['git', 'rev-parse', 'HEAD^{tree}'], source, active, end)
    if c.text(runner.root / ('tree-' + plan['name'] + '.stdout')).strip() != plan['tree']:
        raise ValueError('base tree mismatch')
    for row in plan['files']:
        p = source / row['path']
        if p.stat().st_size != row['bytes'] or c.sha(p) != row['sha256']:
            raise ValueError('base source hash mismatch')
    runner.checked('overlay-' + plan['name'], ['git', 'apply', '--index', str(data / 'diagnostic.patch')], source, active, end)
    runner.checked('effective-' + plan['name'], ['git', 'write-tree'], source, active, end)
    if c.text(runner.root / ('effective-' + plan['name'] + '.stdout')).strip() != plan['effective_overlay_tree']:
        raise ValueError('effective tree mismatch')
    runner.checked('delta-' + plan['name'], ['git', 'diff', '--cached', '--name-only'], source, active, end)
    if c.text(runner.root / ('delta-' + plan['name'] + '.stdout')).splitlines() != ['crates/st3/tests/log_diet.rs']:
        raise ValueError('non-fixture overlay delta')
    if c.sha(source / 'crates/st3/tests/log_diet.rs') != plan['overlay_file']['overlay_file_sha256']:
        raise ValueError('overlay blob mismatch')


def study(args, runner_factory=c.Runner, clock=time.monotonic, prepare=prepare_arm, verify=verify_inputs):
    repo = Path(args.repo).resolve(); root = Path(args.root).resolve()
    record = {}
    runner = runner_factory(root, clock)
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP): signal.signal(sig, runner.interrupt)
    report = {'status': 'NOT_QUALIFIED', 'assignment': args.assignment, 'arms': [], 'commands': [],
              'initial_failure_preserved': False, 'publication_execution_authorized_locally': False}
    final_end = clock()
    try:
        record = c.load(args.deadline)
        c.deadline(record, c.hosted(), clock())
        final_end = record['cutoff']
        if not re.fullmatch(ASSIGNMENT, args.assignment): raise ValueError('immutable assignment input contract mismatch')
        data, plans, manifest = verify(repo)
        report['automation_manifest'] = manifest
        report['caller'] = c.identity()
        report['deadline'] = record
        runner.checked('automation-head', ['git','rev-parse','HEAD','HEAD^{tree}'], repo,
                       record['setup_end'] - 30, record['setup_end'])
        report['automation_identity'] = c.text(root / 'automation-head.stdout').splitlines()
        # Preflight and preparation are charged to the first-step setup budget.
        # Genie also checks every generated file, not only semantic YAML equivalence.
        runner.checked('full-generation', ['nix','develop','--no-update-lock-file','.#genie','-c','genie','--check','--json'], repo,
                       record['setup_end'] - 30, record['setup_end'])
        runner.checked('actionlint', ['nix','develop','--no-update-lock-file','.#genie','-c','python3',str(repo / 'scripts/ci-log-diet-actionlint.py'),'-config-file',
                       str(data / 'actionlint.yaml'), '.github/workflows/perf.yml'], repo,
                       record['setup_end'] - 30, record['setup_end'])
        runner.checked('pure-controls', ['python3','scripts/ci-log-diet-test.py'], repo,
                       record['setup_end'] - 30, record['setup_end'])
        runner.checked('bundle-heads', ['git','bundle','list-heads',str(data / 'source.bundle')], repo,
                       record['setup_end'] - 30, record['setup_end'])
        expected = sorted(p['commit'] + ' refs/heads/frozen/' + p['name'] for p in plans)
        if sorted(c.text(root / 'bundle-heads.stdout').splitlines()) != expected:
            raise ValueError('bundle heads mismatch')
        source_root = root / 'sources'; source_root.mkdir()
        for p in plans:
            prepare(p, source_root / p['name'], data, runner, record['setup_end'] - 30, record['setup_end'])
        if clock() >= record['setup_end']: raise ValueError('setup expired')
        for p in plans:
            arm = root / ('arm-' + p['name']); arm.mkdir()
            # BIND BEFORE nix develop: devshell queue/startup/admission consume this arm.
            window = c.arm_window(record, c.hosted(), clock())
            c.write_json(arm / 'window.json', window); c.write_json(arm / 'plan.json', p)
            c.write_json(arm / 'caller.json', c.identity())
            argv = ['nix','develop','--no-update-lock-file', str(source_root / p['name']), '-c', 'python3',
                    str(repo / 'scripts/ci-log-diet-arm.py'), '--root', str(arm), '--source', str(source_root / p['name']),
                    '--plan', str(arm / 'plan.json'), '--deadline', str(args.deadline),
                    '--window', str(arm / 'window.json'), '--caller', str(arm / 'caller.json')]
            row = runner.run('arm-' + p['name'], argv, repo, window['active'], window['end'])
            child = c.load(arm / 'arm-terminal.json') if (arm / 'arm-terminal.json').exists() else {'status': 'MISSING'}
            report['arms'].append({'source': p['commit'], 'effective_tree': p['effective_overlay_tree'],
                                   'attempt': 1, 'case': c.CASE, 'process': row, 'result': child})
            if child.get('initial') == 'FAIL': report['initial_failure_preserved'] = True
            if (row['exit'] != 0 or row['timed_out'] or row['cancelled'] or not row['output_complete']
                    or not row['cleanup_confirmed'] or child.get('status') != 'COMPLETE'
                    or not child.get('continue') or not child.get('cleanup_confirmed')):
                raise ValueError('arm incomplete; remaining arms NOT RUN')
        if tuple(a['source'] for a in report['arms']) != c.SOURCES: raise ValueError('incomplete source order')
        report['status'] = 'COMPLETE_WITH_FAILURES' if report['initial_failure_preserved'] else 'COMPLETE'
    except BaseException as exc:
        report['refusal_type'] = type(exc).__name__
    finally:
        report['commands'] = runner.commands
        report['cleanup_confirmed'] = c.cleanup(final_end)
        if not report['cleanup_confirmed']: report['status'] = 'NOT_QUALIFIED'
        # Evidence remains even when setup/input/case failed. No automatic next invocation.
        c.write_json(root / 'terminal.json', report)
    return 0 if report['status'] == 'COMPLETE' else 1


def upload_record(args, clock):
    r = c.load(args.deadline)
    now = clock()
    # Validate immutable identity and all phase offsets without pretending the
    # work cutoff is also the upload cutoff.
    c.deadline(r, c.hosted(), r['start'])
    if not math.isfinite(now) or now < r['start'] or now >= r['upload_end']:
        raise ValueError('expired/future upload reserve')
    return r


def retain_worker(args, clock=time.monotonic):
    root = Path(args.root); r = upload_record(args, clock)
    end = min(r['upload_end'] - 120, args.retain_end)
    if clock() >= end: raise ValueError('retention deadline expired')
    rows = []
    # Prune source/build trees BEFORE descending. A blocked stat/read/hash is
    # stopped by the outer owned-process supervisor, not only by these checks.
    with (root / 'artifact-manifest.partial.jsonl').open('x') as progress:
        for directory, dirs, files in os.walk(root, followlinks=False):
            if clock() >= end: raise ValueError('retention deadline expired')
            base = Path(directory)
            dirs[:] = sorted(d for d in dirs if not (base/d).is_symlink() and base/d != root/'sources')
            for name in sorted(files):
                if name.startswith(('artifact-manifest', 'retention-')): continue
                path = base / name
                if clock() >= end: raise ValueError('retention deadline expired')
                if path.is_symlink() or not path.is_file(): continue
                row = {'path': str(path.relative_to(root)), 'bytes': path.stat().st_size, 'sha256': c.sha(path)}
                if clock() >= end: raise ValueError('retention hash crossed deadline')
                rows.append(row); progress.write(__import__('json').dumps(row) + '\n'); progress.flush()
        c.write_json(root / 'artifact-manifest.json', {'files': rows,
                     'terminal_status': c.load(root / 'terminal.json')['status'],
                     'scan_complete': True, 'upload_deadline': r['upload_end']})


def retain(args, runner_factory=c.Runner, clock=time.monotonic):
    root = Path(args.root); r = upload_record(args, clock)
    if not (root / 'terminal.json').exists():
        c.write_json(root / 'terminal.json', {'status': 'NOT_QUALIFIED', 'reason': 'controller did not reach terminal'})
    # At most 300 seconds to hash; 30 seconds to terminate/reap this child;
    # leave at least 90 seconds for the remaining minute-bounded upload.
    active = min(clock() + 300, r['upload_end'] - 120)
    end = min(active + 30, r['upload_end'] - 90)
    if clock() >= active: raise ValueError('insufficient retention plus upload reserve')
    runner = runner_factory(root, clock)
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP): signal.signal(sig, runner.interrupt)
    report = {'retention_complete': False, 'status': 'INCOMPLETE', 'upload_deadline': r['upload_end'],
              'active_deadline': active, 'cleanup_deadline': end, 'partial_preserved': True}
    try:
        row = runner.run('retention-worker', [sys.executable, str(Path(__file__).resolve()), 'retain-worker',
                         '--root', str(root), '--deadline', str(args.deadline), '--retain-end', str(active)],
                         Path(__file__).resolve().parent, active, end)
        report['process'] = row
        complete = (row['exit'] == 0 and not row['timed_out'] and not row['cancelled'] and
                    row['cleanup_confirmed'] and row['output_complete'] and (root/'artifact-manifest.json').exists())
        report['retention_complete'] = complete
        report['cleanup_confirmed'] = row['cleanup_confirmed']
        report['status'] = 'COMPLETE' if complete else 'INCOMPLETE'
    except BaseException as exc:
        report['refusal_type'] = type(exc).__name__
        report['cleanup_confirmed'] = c.cleanup(end)
    finally:
        report['commands'] = runner.commands
        # Original negative/incomplete receipts and partial manifest remain in
        # place. Never turn interruption into COMPLETE or discard those bytes.
        c.write_json(root / 'retention-result.json', report)
        minutes = min(15, int((r['upload_end'] - clock()) // 60))
        if minutes >= 1 and os.environ.get('GITHUB_OUTPUT'):
            with open(os.environ['GITHUB_OUTPUT'], 'a') as f: f.write('upload_minutes=' + str(minutes) + '\n')
    return 0 if report['retention_complete'] else 1


def main():
    p = argparse.ArgumentParser(); p.add_argument('mode', choices=['study','retain','retain-worker'])
    p.add_argument('--repo', default='.'); p.add_argument('--root', required=True)
    p.add_argument('--deadline', required=True); p.add_argument('--assignment', default='')
    p.add_argument('--retain-end', type=float, default=0)
    args = p.parse_args()
    if args.mode == 'retain-worker': retain_worker(args); return 0
    c.subreaper()
    if args.mode == 'retain': return retain(args)
    return study(args)


if __name__ == '__main__': raise SystemExit(main())
