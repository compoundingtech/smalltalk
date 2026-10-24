#!/usr/bin/env python3
"""Networkless snapshot smoke with a stubbed sandbox boundary, not isolation."""
import fcntl
import json
import os
from pathlib import Path
import select
import signal
import shutil
import subprocess
import sys
import tempfile
import unittest

from dependency_cache import INSTALL_INPUTS, install_key
from candidate_sandbox import DEPENDENCY_POLICY


HELPER = Path(__file__).with_name('build-and-swap.sh').resolve()
ROOT = Path(__file__).resolve().parents[4]


class SnapshotLeaseTest(unittest.TestCase):
    def test_shared_snapshot_root_keeps_live_run(self):
        scratch = ROOT / 'tmp'
        scratch.mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='snapshot-lease-', dir=scratch) as temporary:
            fixture = Path(temporary)
            source = fixture / 'source'
            snapshot_root = fixture / 'snapshots'
            cache = fixture / 'dependency-cache'
            binary = fixture / 'bin'
            binary.mkdir()
            # Only the sandbox CLI is intercepted; host Python operations still
            # use the real interpreter. Isolation itself is covered by runner tests.
            python = binary / 'python3'
            python.write_text(f'#!{sys.executable}\n' + '''
import json, os, pathlib, sys
args = sys.argv[1:]
if args and pathlib.Path(args[0]).name == 'candidate_sandbox.py':
    args = args[1:]
    root = pathlib.Path(os.environ['SNAPSHOT_LEASE_FIXTURE'])
    with (root / 'sandbox-calls.jsonl').open('a') as stream:
        stream.write(json.dumps(args) + '\\n')
    if args == ['--check-config']:
        raise SystemExit(0)
    if args == ['--print-policy']:
        print(os.environ['SNAPSHOT_LEASE_POLICY'])
        raise SystemExit(0)
    assert len(args) == 15, args
    assert args[0] == '--worktree' and args[2:5] == ['--shell', 'web', '--output'], args
    assert args[6:8] == ['--readonly-dependencies', '--directory'], args
    assert args[9:12] == ['--', 'node', 'private/build-live.mjs'], args
    worktree, output = pathlib.Path(args[1]), pathlib.Path(args[5])
    assert worktree.is_absolute() and output.is_absolute(), args
    assert args[8] == str(worktree / 'apps/fractal-web'), args
    assert args[12] == str(output), args
    os.chdir(args[8])
    os.environ['SNAPSHOT_LEASE_SANDBOX'] = '1'
    os.execvp(args[10], args[10:])
'''+ f'\nos.execv({sys.executable!r}, [{sys.executable!r}, *args])\n')
            python.chmod(0o755)
            for relative in ('apps/fractal-web/private/build-live.mjs',
                             'apps/fractal-web/vendor/utils/src/node/cli-build-identity.js',
                             'node_modules/fixture/index.js'):
                path = source / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('// synthetic fixture\n')
            for name in INSTALL_INPUTS:
                (source / name).write_text(name + '\n')
            (source / 'megarepo.lock').write_text('{"members": {}}')
            git = binary / 'git'
            git.write_text('''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
root = pathlib.Path(os.environ['SNAPSHOT_LEASE_FIXTURE'])
args = sys.argv[3:]
with (root / 'git-calls.jsonl').open('a') as stream:
    stream.write(json.dumps(args) + '\\n')
if args[0] == 'rev-parse':
    print(os.environ['SNAPSHOT_LEASE_SHA'])
elif args[:2] == ['worktree', 'add']:
    (pathlib.Path(args[3]) / 'apps/fractal-web').mkdir(parents=True)
elif args[:2] == ['worktree', 'remove']:
    target = pathlib.Path(args[3])
    assert any(target.is_relative_to(root / name) for name in ('snapshots', 'relative-snapshot-root'))
    shutil.rmtree(target)
else:
    raise SystemExit(f'unexpected git arguments: {args}')
''')
            node = binary / 'node'
            node.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
assert os.environ.get('SNAPSHOT_LEASE_SANDBOX') == '1', 'builder bypassed sandbox CLI'
if os.environ.get('SNAPSHOT_LEASE_HOLD') == '1':
    print('READY:' + os.getcwd(), flush=True)
    with pathlib.Path(os.environ['SNAPSHOT_LEASE_RELEASE']).open() as stream:
        stream.read(1)
build, sha = pathlib.Path(sys.argv[2]), sys.argv[4]
(build / 'web').mkdir(parents=True)
(build / 'web/index.html').write_text('<script src="/assets/fixture.js"></script>')
(build / 'launch.sh').write_text('#!/bin/sh\\n')
(build / 'build-receipt.json').write_text(json.dumps({
    'buildIdentity': {'rev': sha, 'dirty': False}, 'stackHead': sha}))
''')
            git.chmod(0o755)
            node.chmod(0o755)
            # Emulate the deployment wrapper's getcwd guard. The old EXIT trap
            # removed its own cwd before this command and therefore failed.
            rm = binary / 'rm'
            rm.write_text(f'''#!{sys.executable}
import os, pathlib, sys
try:
    os.getcwd()
except FileNotFoundError:
    raise SystemExit("command wrapper refuses deleted cwd")
if os.environ.get("SNAPSHOT_CLEANUP_FAIL_LOCK") == "1" and sys.argv[-1].endswith(".lock"):
    raise SystemExit("injected recoverable lock-file cleanup failure")
os.execv({shutil.which('rm')!r}, ["rm", *sys.argv[1:]])
''')
            rm.chmod(0o755)
            mv = binary / 'mv'
            mv.write_text(f'''#!{sys.executable}
import os, pathlib, sys
if os.environ.get("SNAPSHOT_FAIL_ROLLBACK") == "1":
    src, dst = pathlib.Path(sys.argv[-2]), pathlib.Path(sys.argv[-1])
    if src.name.startswith(".current-rollback-"):
        raise SystemExit("injected rollback rename failure")
    if src.name.startswith(".current-") and dst.name == "current":
        # Activation succeeds, then the shell's current-sha write fails.
        pid = src.name.removeprefix(".current-")
        (dst.parent / (".current-sha-" + pid)).mkdir()
os.execv({shutil.which('mv')!r}, ["mv", *sys.argv[1:]])
''')
            mv.chmod(0o755)
            release = fixture / 'release'
            os.mkfifo(release)
            base_env = {
                **os.environ,
                'PATH': str(binary) + ':' + os.environ['PATH'],
                'SNAPSHOT_LEASE_FIXTURE': str(fixture),
                'SNAPSHOT_LEASE_RELEASE': str(release),
                'SNAPSHOT_LEASE_POLICY': DEPENDENCY_POLICY,
                'FRACTAL_BUILD_SNAPSHOT_ROOT': str(snapshot_root),
                'FRACTAL_DEP_CACHE': str(cache),
                'FRACTAL_BUILD_ADMISSION': 'fallback',
            }
            first_sha, second_sha = 'a' * 40, 'b' * 40
            first_env = {**base_env, 'FRACTAL_BUILD_ROOT': str(fixture / 'build-root-a'),
                         'SNAPSHOT_LEASE_SHA': first_sha, 'SNAPSHOT_LEASE_HOLD': '1'}
            first = subprocess.Popen(['bash', str(HELPER), str(source), first_sha, 'shared'],
                                     env=first_env, text=True, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, start_new_session=True)
            try:
                self.assertTrue(select.select([first.stdout], [], [], 10)[0], 'first build never became ready')
                ready = first.stdout.readline().strip()
                self.assertTrue(ready.startswith('READY:'), ready)
                live_snapshot = Path(ready.removeprefix('READY:')).parents[1]
                self.assertEqual(live_snapshot.parent, snapshot_root / 'shared')
                self.assertTrue(live_snapshot.is_dir())
                with Path(str(live_snapshot) + '.lock').open() as lease:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                key = install_key(source)
                generation = cache / DEPENDENCY_POLICY / key
                self.assertFalse((cache / key).exists(), 'sandbox reused an unnamespaced parent generation')
                dependency = generation / 'node_modules/fixture/index.js'
                original_stat = dependency.stat()
                self.assertNotEqual(original_stat.st_ino, (source / 'node_modules/fixture/index.js').stat().st_ino)
                self.assertEqual((live_snapshot / 'node_modules/fixture/index.js').stat().st_ino, original_stat.st_ino)
                with (generation / '.lease').open() as lease:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                (source / 'node_modules/fixture/index.js').write_text('mutable source changed\n')
                second_key = install_key(source)
                self.assertNotEqual(second_key, key)
                second_env = {**base_env, 'FRACTAL_BUILD_ROOT': str(fixture / 'build-root-b'),
                              'SNAPSHOT_LEASE_SHA': second_sha, 'SNAPSHOT_LEASE_HOLD': '0',
                              'SNAPSHOT_CLEANUP_FAIL_LOCK': '1'}
                second = subprocess.run(['bash', str(HELPER), str(source), second_sha, 'shared'],
                                        env=second_env, capture_output=True, text=True, timeout=15)
                self.assertEqual(second.returncode, 0, second.stderr)
                self.assertTrue(live_snapshot.is_dir(), 'second build reclaimed the live first snapshot')
                self.assertEqual(dependency.read_text(), '// synthetic fixture\n')
                self.assertEqual(dependency.stat().st_ino, original_stat.st_ino)
                self.assertEqual(dependency.stat().st_mtime_ns, original_stat.st_mtime_ns)
                receipt = json.loads((fixture / 'build-root-b/shared/builds' / second_sha / 'build-receipt.json').read_text())
                self.assertEqual(receipt['dependencyKey'], second_key)
                self.assertEqual(receipt['dependencyFiles'], {'linked': 1, 'copied': 0})
                logged = json.loads((fixture / 'build-root-b/shared/builds.jsonl').read_text())
                self.assertEqual(logged['outcome'], 'built')
                self.assertIn('cannot remove snapshot lock file', logged['cleanupWarnings'])
                calls = [json.loads(line) for line in (fixture / 'git-calls.jsonl').read_text().splitlines()]
                self.assertFalse(any(args[:2] == ['worktree', 'remove'] and args[3] == str(live_snapshot)
                                     for args in calls))
                with release.open('w') as stream:
                    stream.write('x')
                _, errors = first.communicate(timeout=15)
                self.assertEqual(first.returncode, 0, errors)
                self.assertFalse(live_snapshot.exists())
                self.assertTrue((fixture / 'build-root-a/shared/builds' / first_sha).is_dir())
                self.assertTrue((fixture / 'build-root-b/shared/builds' / second_sha).is_dir())
                relative_sha = 'c' * 40
                relative_env = {**base_env, 'FRACTAL_BUILD_ROOT': 'relative-build-root',
                                'FRACTAL_BUILD_SNAPSHOT_ROOT': 'relative-snapshot-root',
                                'FRACTAL_DEP_CACHE': 'dependency-cache',
                                'SNAPSHOT_LEASE_SHA': relative_sha, 'SNAPSHOT_LEASE_HOLD': '0'}
                relative = subprocess.run(['bash', str(HELPER), str(source), relative_sha, 'shared'],
                                          cwd=fixture, env=relative_env, capture_output=True,
                                          text=True, timeout=15)
                self.assertEqual(relative.returncode, 0, relative.stderr)
                relative_build = fixture / 'relative-build-root/shared/builds' / relative_sha
                self.assertEqual((fixture / 'relative-build-root/shared/current').resolve(), relative_build)
                receipt = json.loads((relative_build / 'build-receipt.json').read_text())
                self.assertEqual(receipt['buildIdentity']['rev'], relative_sha)
                self.assertEqual(receipt['dependencyKey'], second_key)
                self.assertTrue((fixture / 'relative-build-root/shared/builds.jsonl').is_file())
                self.assertEqual(list((fixture / 'relative-snapshot-root/shared').iterdir()), [])
                failed_sha = 'd' * 40
                failed_env = {**relative_env, 'SNAPSHOT_LEASE_SHA': failed_sha,
                              'SNAPSHOT_FAIL_ROLLBACK': '1'}
                failed = subprocess.run(['bash', str(HELPER), str(source), failed_sha, 'shared'],
                                        cwd=fixture, env=failed_env, capture_output=True,
                                        text=True, timeout=15)
                self.assertNotEqual(failed.returncode, 0)
                production = fixture / 'relative-build-root/shared'
                preserved = production / 'builds' / failed_sha
                self.assertEqual((production / 'current').resolve(strict=True), preserved)
                self.assertTrue(preserved.is_dir(), 'failed rollback deleted the selected artifact')
                self.assertEqual((production / 'current-sha').read_text().strip(), relative_sha)
                failure_log = json.loads((production / 'builds.jsonl').read_text().splitlines()[-1])
                self.assertEqual(failure_log['outcome'], 'failed')
                self.assertEqual(failure_log['reason'], 'rollback-failed')
                self.assertTrue(any('preserving failed build' in warning
                                    for warning in failure_log['cleanupWarnings']))
                sandbox_calls = [json.loads(line) for line in
                                 (fixture / 'sandbox-calls.jsonl').read_text().splitlines()]
                self.assertEqual(sandbox_calls.count(['--check-config']), 4)
                self.assertEqual(sandbox_calls.count(['--print-policy']), 4)
                executions = [args for args in sandbox_calls if args not in (['--check-config'], ['--print-policy'])]
                self.assertEqual(len(executions), 4)
                self.assertEqual({args[5] for args in executions},
                                 {str(fixture / 'build-root-a/shared/builds' / first_sha),
                                  str(fixture / 'build-root-b/shared/builds' / second_sha),
                                  str(relative_build), str(preserved)})
            finally:
                if first.poll() is None:
                    os.killpg(first.pid, signal.SIGTERM)
                    try:
                        first.communicate(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(first.pid, signal.SIGKILL)
                        first.communicate()


if __name__ == '__main__':
    unittest.main(verbosity=2)
