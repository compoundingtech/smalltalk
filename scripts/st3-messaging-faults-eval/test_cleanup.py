"""Kill a real, isolated messaging eval; prove processes AND private roots disappear.

Cargo supplies the fixture binary. Running unittest directly covers the small
failure-path checks; set ST3_MFE_TEST_BINARY to also run the native cleanup proof.
"""
import asyncio
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, patch

HERE = Path(__file__).resolve().parent
loader = importlib.machinery.SourceFileLoader('cleanup_eval', str(HERE / 'run'))
runner = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
loader.exec_module(runner)


def wait_for(predicate, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError('cleanup fixture deadline expired')


def descendants(pid):
    result = set()
    def visit(parent):
        try:
            # Rust spawns the provider on a blocking-runtime thread. Linux's
            # children file belongs to a thread, not to the whole process.
            children = {int(child) for task in Path(f'/proc/{parent}/task').iterdir()
                        for child in (task / 'children').read_text().split()}
        except FileNotFoundError:
            return
        for child in children:
            result.add(child)
            visit(child)
    visit(pid)
    return result


class CleanupTests(unittest.TestCase):
    def test_evidence_failure_still_cleans_both_nodes_and_roots(self):
        with tempfile.TemporaryDirectory() as directory:
            parent = Path(directory)
            args = SimpleNamespace(scratch=parent, keep=False)
            nodes = [SimpleNamespace(cleanup=AsyncMock(), name=name, root=parent / name) for name in ('amber', 'cobalt')]
            with patch.dict(os.environ, ST3_MESSAGING_ROOT=directory), \
                 patch.object(runner, 'Node', side_effect=nodes), \
                 patch.object(runner, 'free_port', side_effect=OSError('setup failed')), \
                 patch.object(runner, 'agent', new=AsyncMock(side_effect=OSError('evidence failed'))), \
                 patch.object(runner, 'write_json', side_effect=OSError('disk full')):
                with self.assertRaisesRegex(OSError, 'disk full'):
                    asyncio.run(runner.run_case(args, 'baseline', parent, None, None, None))
            for node in nodes:
                node.cleanup.assert_awaited_once()
            self.assertEqual(list(parent.iterdir()), [])

    def test_provider_shutdown_bounds_a_stuck_extension_and_kills_its_channel(self):
        with tempfile.TemporaryDirectory(prefix='mfe-provider-proof-') as directory:
            root = Path(directory)
            extension = root / 'extension.mjs'
            extension.write_text('''
import childProcess from 'node:child_process';
import fs from 'node:fs';
export default api => {
  api.on('session_start', async () => {
    const child = childProcess.spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)']);
    fs.writeFileSync('child.pid', String(child.pid));
  });
  api.on('session_shutdown', () => new Promise(() => {}));
};
''')
            with (root / 'provider.log').open('wb') as log:
                launcher = subprocess.Popen([sys.executable, str(HERE.parent / 'st3_test_process.py'),
                    '--', 'node', str(HERE / 'provider.mjs'), '-e', str(extension)],
                    cwd=root, stdout=log, stderr=log)
                try:
                    def ready():
                        receipts = root / 'receipts.jsonl'
                        if not receipts.exists():
                            return None
                        rows = [json.loads(line) for line in receipts.read_text().splitlines()]
                        return rows if any(row['event'] == 'ready' for row in rows) else None
                    rows = wait_for(ready, 5)
                    provider = rows[0]['pid']
                    child = int((root / 'child.pid').read_text())
                    self.assertEqual(os.getpgid(child), child)
                    os.kill(provider, signal.SIGTERM)
                    launcher.wait(timeout=3)
                    self.assertEqual(launcher.returncode, 1, (root / 'provider.log').read_text())
                    wait_for(lambda: not Path(f'/proc/{child}').exists(), 2)
                finally:
                    if launcher.poll() is None:
                        launcher.kill()
                        launcher.wait()

    @unittest.skipUnless(os.environ.get('ST3_MFE_TEST_BINARY'), 'Cargo supplies the isolated daemon binary')
    def test_controller_death_reaps_native_tree_and_private_directories(self):
        binary = str(Path(os.environ['ST3_MFE_TEST_BINARY']).resolve())
        for target, sig, keep in [('controller', signal.SIGKILL, False),
                                  ('launcher', signal.SIGKILL, False),
                                  ('controller', signal.SIGTERM, False),
                                  ('controller', signal.SIGKILL, True)]:
            with self.subTest(target=target, signal=sig, keep=keep), \
                 tempfile.TemporaryDirectory(prefix='mfe-cleanup-proof-') as directory:
                parent = Path(directory)
                scratch = parent / 'scratch'
                evidence = parent / 'evidence'
                env = dict(os.environ)
                for key in ('ST_AGENT', 'ST3_SUBJECT', 'ST3_MESSAGING_ROOT', 'ST3_MESSAGING_SCRATCH'):
                    env.pop(key, None)
                command = [sys.executable, str(HERE / 'run'), binary, str(evidence),
                           '--old-binary', binary, '--scratch', str(scratch), '--cases', 'baseline']
                if keep:
                    command.append('--keep')
                roots = []
                fds = []
                with (parent / 'eval.log').open('wb') as log:
                    launcher = subprocess.Popen(command, env=env, stdout=log, stderr=log)
                    try:
                        def ready():
                            self.assertIsNone(launcher.poll(), (parent / 'eval.log').read_text())
                            # Locate only this invocation by its unique evidence argument.
                            for proc in Path('/proc').glob('[0-9]*'):
                                try:
                                    argv = (proc / 'cmdline').read_bytes().split(b'\0')
                                    if str(evidence).encode() not in argv:
                                        continue
                                    values = dict(row.split(b'=', 1) for row in (proc / 'environ').read_bytes().split(b'\0') if b'=' in row)
                                    root = values.get(b'ST3_MESSAGING_ROOT')
                                    if not root:
                                        continue
                                    root = Path(os.fsdecode(root))
                                    roots[:] = [root, Path(os.fsdecode(values[b'ST3_MESSAGING_SCRATCH']))]
                                    receipt_files = list(root.glob('mfe-*/cobalt/ws/receipts.jsonl'))
                                    if not receipt_files:
                                        continue
                                    receipts = [json.loads(line) for line in receipt_files[0].read_text().splitlines()]
                                    if not any(row['event'] == 'ready' for row in receipts):
                                        continue
                                    guardian = int(values[b'SMALLTALK_TEST_SUPERVISOR'])
                                    pids = descendants(guardian)
                                    commands = [Path(f'/proc/{pid}/cmdline').read_bytes().replace(b'\0', b' ') for pid in pids]
                                    for token, count in ((b' up ', 2), (b' replication-worker ', 2),
                                                         (b' driver omp ', 1), (b'provider.mjs', 1),
                                                         (b' omp-channel ', 1)):
                                        if sum(token in cmd for cmd in commands) < count:
                                            return None
                                    return int(proc.name), guardian, root, Path(os.fsdecode(values[b'ST3_MESSAGING_SCRATCH'])), pids
                                except (OSError, ValueError, KeyError):
                                    continue
                        controller, guardian, root, binary_root, pids = wait_for(ready, 90)
                        roots = [root, binary_root]
                        for pid in pids:
                            try:
                                fds.append(os.pidfd_open(pid))
                            except ProcessLookupError:
                                pass
                        os.kill(controller if target == 'controller' else launcher.pid, sig)
                        launcher.wait(timeout=10)
                        wait_for(lambda: all(not Path(f'/proc/{pid}').exists() for pid in pids), 5)
                        wait_for(lambda: not Path(f'/proc/{guardian}').exists(), 5)
                        if keep:
                            self.assertTrue(all(path.is_dir() for path in roots))
                        else:
                            self.assertTrue(all(not path.exists() for path in roots))
                        self.assertEqual(list(scratch.iterdir()), [binary_root] if keep else [])
                    finally:
                        if launcher.poll() is None:
                            launcher.kill()
                            launcher.wait()
                        for fd in fds:
                            try:
                                signal.pidfd_send_signal(fd, signal.SIGKILL)
                            except ProcessLookupError:
                                pass
                            os.close(fd)
                        import shutil
                        for root in roots:
                            shutil.rmtree(root, ignore_errors=True)


if __name__ == '__main__':
    unittest.main()
