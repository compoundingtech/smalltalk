#!/usr/bin/env python3
"""Filesystem coverage for preparing snapshots from immutable dependency generations."""
import contextlib
import errno
import fcntl
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from dependency_cache import _make_directories_writable, install_key, populate


HELPER = Path(__file__).with_name('prepare-dependencies.py').resolve()
SPEC = importlib.util.spec_from_file_location('prepare_dependencies', HELPER)
PREPARATION = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREPARATION)


class PrepareDependenciesTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='prepare-dependencies-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.addCleanup(lambda: _make_directories_writable(self.root))
        self.source = self.root / 'source'
        self.snapshot = self.root / 'snapshot'
        self.cache = self.root / 'cache'
        self.manifest = self.root / 'manifest.json'
        self.source.mkdir()
        self.snapshot.mkdir()
        self.cache.mkdir()
        for name in ('pnpm-lock.yaml', 'pnpm-workspace.yaml', 'megarepo.kdl',
                     'megarepo.lock', 'pnpm-source-inputs.json'):
            (self.source / name).write_text(name + '\n')
        (self.source / 'megarepo.lock').write_text(json.dumps({'members': {}}))
        for root in (self.source, self.snapshot):
            workspace = root / 'packages/workspace'
            workspace.mkdir(parents=True)
            (workspace / 'index.js').write_text('export const workspace = true;\n')
        self.write_source('node_modules/fixture/index.js', 'immutable root dependency\n')
        self.write_source('apps/fractal-web/node_modules/nested/index.js', 'immutable nested dependency\n')
        self.write_source('apps/fractal-web/private/build-live.mjs', '// ignored overlay\n')
        self.write_source('apps/fractal-web/private/node_modules/tool/index.js', '// cached overlay dependency\n')
        self.write_source('apps/fractal-web/vendor/utils/index.js', '// vendor overlay\n')
        (self.source / 'apps/fractal-web/private/build-link.mjs').symlink_to('build-live.mjs')
        (self.source / 'node_modules/workspace').symlink_to('../packages/workspace')
        (self.source / 'apps/fractal-web/node_modules/workspace').symlink_to('../../../packages/workspace')
        (self.source / 'node_modules/absolute-workspace').symlink_to(self.source / 'packages/workspace')
        self.key = install_key(self.source)
        with (self.cache / '.cache.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            populate(self.source, self.cache, self.key)
        self.generation = self.cache / self.key

    def write_source(self, relative, content):
        path = self.source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def prepare(self):
        PREPARATION.prepare(self.source, self.snapshot, self.manifest, self.generation, self.key)

    def test_cli_links_cache_files_and_preserves_overlay_manifest(self):
        source_file = self.source / 'node_modules/fixture/index.js'
        result = subprocess.run([sys.executable, str(HELPER), str(self.source), str(self.snapshot),
                                 str(self.manifest), str(self.generation), self.key],
                                text=True, capture_output=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, '')
        snapshot_file = self.snapshot / 'node_modules/fixture/index.js'
        cache_file = self.generation / 'node_modules/fixture/index.js'
        self.assertEqual(snapshot_file.read_text(), 'immutable root dependency\n')
        self.assertTrue(os.path.samefile(snapshot_file, cache_file))
        self.assertFalse(os.path.samefile(snapshot_file, source_file))
        for relative in ('node_modules/workspace', 'node_modules/absolute-workspace',
                         'apps/fractal-web/node_modules/workspace'):
            link = self.snapshot / relative
            self.assertTrue(link.is_symlink())
            self.assertEqual(link.resolve(), self.snapshot / 'packages/workspace')
        manifest = json.loads(self.manifest.read_text())
        self.assertEqual(manifest['dependencyKey'], self.key)
        self.assertEqual(manifest['dependencyFiles'], {'linked': 3, 'copied': 0})
        overlay = manifest['copiedIgnoredOverlayFiles']
        self.assertEqual(overlay['apps/fractal-web/private/build-live.mjs'], {
            'sha256': hashlib.sha256(b'// ignored overlay\n').hexdigest()})
        self.assertEqual(overlay['apps/fractal-web/private/build-link.mjs'], {'symlink': 'build-live.mjs'})
        self.assertIn('apps/fractal-web/vendor/utils/index.js', overlay)
        self.assertFalse(any('node_modules' in name for name in overlay))
        self.assertIn(f'key={self.key} linked=3 copied=0', result.stderr)

    def test_in_place_snapshot_write_is_denied_and_directories_are_private(self):
        self.prepare()
        snapshot_file = self.snapshot / 'node_modules/fixture/index.js'
        cache_file = self.generation / 'node_modules/fixture/index.js'
        self.assertTrue(os.path.samefile(snapshot_file, cache_file))
        self.assertEqual(snapshot_file.stat().st_mode & 0o222, 0)
        self.assertNotEqual(snapshot_file.parent.stat().st_ino, cache_file.parent.stat().st_ino)
        self.assertTrue(snapshot_file.parent.stat().st_mode & 0o200)
        self.assertEqual(cache_file.parent.stat().st_mode & 0o222, 0)
        # Root bypasses DAC, so exercise the actual open as an unprivileged UID.
        self.root.chmod(0o755)
        script = '''
import errno, os, sys
if os.geteuid() == 0:
    os.setgroups([])
    os.setgid(65534)
    os.setuid(65534)
try:
    with open(sys.argv[1], 'w') as stream:
        stream.write('must not modify the cache')
except OSError as error:
    assert error.errno == errno.EACCES, error
else:
    raise AssertionError('in-place snapshot write succeeded')
'''
        result = subprocess.run([sys.executable, '-c', script, str(snapshot_file)],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(cache_file.read_text(), 'immutable root dependency\n')
        # New build output can replace a directory entry without editing the
        # shared inode, and subsequent snapshots can still reuse the generation.
        snapshot_file.unlink()
        snapshot_file.write_text('snapshot-only replacement\n')
        self.assertEqual(cache_file.read_text(), 'immutable root dependency\n')
        with (self.cache / '.cache.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            populate(self.source, self.cache, self.key)

    def test_reuses_clone_a_generation_with_clone_b_pinned_workspace(self):
        source_a = self.source
        source_b = self.root / 'source-b'
        # Clone reuse requires the same literal links and installed-tree stats.
        absolute = source_a / 'node_modules/absolute-workspace'
        absolute.unlink()
        absolute.symlink_to('../packages/workspace')
        self.key = install_key(source_a)
        with (self.cache / '.cache.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            populate(source_a, self.cache, self.key)
        self.generation = self.cache / self.key
        shutil.copytree(source_a, source_b, symlinks=True, copy_function=shutil.copy2)
        (source_a / 'packages/workspace/index.js').write_text('clone A mutable workspace\n')
        (source_b / 'packages/workspace/index.js').write_text('clone B mutable workspace\n')
        (self.snapshot / 'packages/workspace/index.js').write_text('clone B pinned workspace\n')
        self.assertEqual(install_key(source_b), self.key)
        self.source = source_b

        self.prepare()

        for relative in ('node_modules/workspace', 'node_modules/absolute-workspace',
                         'apps/fractal-web/node_modules/workspace'):
            with self.subTest(relative=relative):
                link = self.snapshot / relative
                self.assertTrue(link.is_symlink())
                self.assertFalse(Path(os.readlink(link)).is_absolute())
                self.assertNotIn(str(source_a), os.readlink(link))
                self.assertEqual(link.resolve(), self.snapshot / 'packages/workspace')
                self.assertEqual((link / 'index.js').read_text(), 'clone B pinned workspace\n')
        self.assertEqual((self.snapshot / 'node_modules/fixture/index.js').read_text(),
                         'immutable root dependency\n')
        self.assertEqual(json.loads(self.manifest.read_text())['dependencyKey'], self.key)

    def test_rejects_workspace_symlink_ancestor_outside_snapshot(self):
        outside = self.root / 'outside'
        (self.snapshot / 'packages').rename(outside)
        (self.snapshot / 'packages').symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'dependency symlink escapes snapshot and generation'):
            self.prepare()
        self.assertFalse(self.manifest.exists())

    def test_allows_dangling_workspace_links_within_snapshot(self):
        shutil.rmtree(self.snapshot / 'packages/workspace')

        self.prepare()

        for relative in ('node_modules/workspace', 'node_modules/absolute-workspace',
                         'apps/fractal-web/node_modules/workspace'):
            link = self.snapshot / relative
            self.assertTrue(link.is_symlink())
            self.assertFalse(link.exists())
            self.assertEqual(link.resolve(strict=False), self.snapshot / 'packages/workspace')

    def test_rejects_marker_mismatch_before_copying(self):
        (self.generation / '.fractal-dep-key').chmod(0o644)
        (self.generation / '.fractal-dep-key').write_text('0' * 64 + '\n')
        with self.assertRaises(ValueError):
            self.prepare()
        self.assertEqual(list(self.snapshot.iterdir()), [self.snapshot / 'packages'])
        self.assertFalse(self.manifest.exists())

    def test_rejects_changed_source_key_before_copying(self):
        (self.source / 'pnpm-lock.yaml').write_text('changed\n')
        with self.assertRaisesRegex(ValueError, 'source install key'):
            self.prepare()
        self.assertFalse((self.snapshot / 'node_modules').exists())
        self.assertFalse((self.snapshot / 'apps').exists())

    def test_source_mutation_rejects_stale_key_and_refreshes_independently(self):
        old_key, old_generation = self.key, self.generation
        old_file = old_generation / 'node_modules/fixture/index.js'
        old_info = old_file.stat()
        self.write_source('node_modules/fixture/index.js', 'refreshed installed dependency\n')
        with self.assertRaisesRegex(ValueError, 'source install key'):
            self.prepare()
        self.assertFalse((self.snapshot / 'node_modules').exists())
        self.assertFalse(self.manifest.exists())
        self.assertEqual(old_file.read_text(), 'immutable root dependency\n')
        self.assertEqual((old_file.stat().st_ino, old_file.stat().st_mtime_ns,
                          old_file.stat().st_mode, old_file.stat().st_size),
                         (old_info.st_ino, old_info.st_mtime_ns, old_info.st_mode, old_info.st_size))
        self.key = install_key(self.source)
        self.assertNotEqual(self.key, old_key)
        with (self.cache / '.cache.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            populate(self.source, self.cache, self.key)
        self.generation = self.cache / self.key
        self.prepare()
        snapshot_file = self.snapshot / 'node_modules/fixture/index.js'
        self.assertEqual(snapshot_file.read_text(), 'refreshed installed dependency\n')
        self.assertTrue(os.path.samefile(
            snapshot_file, self.generation / 'node_modules/fixture/index.js'))
        self.assertEqual(old_file.read_text(), 'immutable root dependency\n')
        self.assertEqual((old_file.stat().st_ino, old_file.stat().st_mtime_ns,
                          old_file.stat().st_mode, old_file.stat().st_size),
                         (old_info.st_ino, old_info.st_mtime_ns, old_info.st_mode, old_info.st_size))
        self.assertEqual(json.loads(self.manifest.read_text())['dependencyKey'], self.key)

    def test_rejects_unsafe_inventory(self):
        (self.generation / '.node-modules.json').chmod(0o644)
        for inventory in (['../outside/node_modules'], ['/outside/node_modules'],
                          ['packages/../../outside/node_modules'], ['packages/workspace'],
                          {'node_modules': True}, [7]):
            with self.subTest(inventory=inventory):
                (self.generation / '.node-modules.json').write_text(json.dumps(inventory))
                with self.assertRaises(ValueError):
                    self.prepare()
                self.assertFalse((self.snapshot / 'node_modules').exists())

    def test_rejects_escaping_inventory_ancestor(self):
        self.generation.chmod(0o755)
        (self.generation / '.node-modules.json').chmod(0o644)
        outside = self.root / 'outside'
        (outside / 'node_modules').mkdir(parents=True)
        (self.generation / 'escape').symlink_to(outside, target_is_directory=True)
        (self.generation / '.node-modules.json').write_text(json.dumps(['escape/node_modules']))
        with self.assertRaises(ValueError):
            self.prepare()
        self.assertFalse((self.snapshot / 'escape').exists())

    def test_rejects_outer_overlay_symlink(self):
        private = self.source / 'apps/fractal-web/private'
        shutil.rmtree(private)
        outside = self.root / 'outside'
        outside.mkdir()
        (outside / 'secret').write_text('not an overlay\n')
        private.symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'source overlay'):
            self.prepare()
        self.assertFalse((self.snapshot / 'apps').exists())

    def test_rejects_escaping_overlay_ancestor(self):
        apps = self.source / 'apps'
        apps.rename(self.root / 'outside-apps')
        apps.symlink_to(self.root / 'outside-apps', target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'source overlay'):
            self.prepare()
        self.assertFalse((self.snapshot / 'apps').exists())

    def test_rejects_destination_symlink_ancestor(self):
        outside = self.root / 'outside'
        outside.mkdir()
        (self.snapshot / 'apps').symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'destination'):
            self.prepare()
        self.assertEqual(list(outside.iterdir()), [])
        self.assertFalse((self.snapshot / 'node_modules').exists())

    def test_rejects_destination_root_symlink(self):
        (self.snapshot / 'node_modules').symlink_to(self.root / 'outside', target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'destination'):
            self.prepare()
        self.assertFalse((self.snapshot / 'apps').exists())

    def test_exdev_warns_before_copying_and_counts_fallback(self):
        errors = io.StringIO()
        copy2 = shutil.copy2
        copied = []

        def observed_copy(src, dst, *args, **kwargs):
            if Path(src).is_relative_to(self.generation):
                self.assertIn('WARNING: EXDEV dependency copy fallback', errors.getvalue())
                copied.append(Path(dst))
            return copy2(src, dst, *args, **kwargs)

        with contextlib.redirect_stderr(errors), \
                patch.object(PREPARATION.os, 'link', side_effect=OSError(errno.EXDEV, 'cross-device')), \
                patch.object(PREPARATION.shutil, 'copy2', side_effect=observed_copy):
            self.prepare()
        self.assertEqual(len(copied), 3)
        manifest = json.loads(self.manifest.read_text())
        self.assertEqual(manifest['dependencyFiles'], {'linked': 0, 'copied': 3})
        self.assertIn('linked=0 copied=3 WARNING: EXDEV copy fallback', errors.getvalue())
        self.assertEqual(errors.getvalue().count('WARNING: EXDEV dependency copy fallback'), 1)
        self.assertFalse(os.path.samefile(self.snapshot / 'node_modules/fixture/index.js',
                                         self.generation / 'node_modules/fixture/index.js'))

    def test_other_link_errors_propagate_without_fallback(self):
        errors = io.StringIO()
        copy2 = shutil.copy2
        dependency_copies = []

        def observed_copy(src, dst, *args, **kwargs):
            if Path(src).is_relative_to(self.generation):
                dependency_copies.append(Path(dst))
            return copy2(src, dst, *args, **kwargs)

        with contextlib.redirect_stderr(errors), \
                patch.object(PREPARATION.os, 'link', side_effect=OSError(errno.EACCES, 'denied')), \
                patch.object(PREPARATION.shutil, 'copy2', side_effect=observed_copy):
            with self.assertRaises(shutil.Error) as caught:
                self.prepare()
        self.assertIn('denied', str(caught.exception))
        self.assertEqual(dependency_copies, [])
        self.assertNotIn('EXDEV', errors.getvalue())
        self.assertFalse(self.manifest.exists())


    def populate_members(self, members):
        (self.source / 'megarepo.lock').write_text(json.dumps({'members': members}))
        self.key = install_key(self.source)
        with (self.cache / '.cache.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            populate(self.source, self.cache, self.key)
        self.generation = self.cache / self.key

    def test_selected_member_sources_and_cached_dependencies_are_snapshot_local(self):
        relative = Path('.devenv/candidate-members/github.com/example/member/refs/commits/pinned')
        member = self.source / relative
        self.write_source(relative / 'package.json', '{"name":"member"}')
        self.write_source(relative / 'node_modules/dependency/index.js', 'cached member dependency\n')
        self.write_source(relative / 'packages/workspace/node_modules/nested/index.js',
                          'cached nested member dependency\n')
        self.write_source(relative / 'packages/workspace/index.js', '// member workspace\n')
        self.write_source(relative / 'packages/workspace/.git/config', 'must not copy\n')
        (member / '.git').symlink_to(self.root / 'external-git-metadata')
        (member / 'workspace').symlink_to(member / 'packages/workspace')
        (member / 'node_modules/workspace').symlink_to('../packages/workspace')
        (self.source / 'repos').mkdir()
        (self.source / 'repos/member').symlink_to(member)
        (self.source / 'node_modules/member').symlink_to(member)
        (self.source / 'apps/fractal-web/node_modules/member').symlink_to('../../../repos/member')
        unselected = Path('.devenv/candidate-members/unselected')
        self.write_source(unselected / 'secret', 'not selected\n')
        self.write_source(unselected / 'node_modules/secret/index.js', 'not selected dependency\n')
        self.populate_members({'member': {}})
        # Refreshed member installs invalidate the old fingerprint before any
        # snapshot copying; the immutable old generation remains untouched.
        old_generation, old_key = self.generation, self.key
        self.write_source(relative / 'node_modules/dependency/index.js', 'changed source dependency\n')
        self.write_source(relative / 'packages/late/node_modules/uncached/index.js', 'not cached\n')
        with self.assertRaisesRegex(ValueError, 'source install key'):
            self.prepare()
        self.assertFalse(self.manifest.exists())
        self.assertEqual((old_generation / relative / 'node_modules/dependency/index.js').read_text(),
                         'cached member dependency\n')
        self.populate_members({'member': {}})
        self.assertNotEqual(self.key, old_key)

        self.prepare()

        copied = self.snapshot / relative
        source_package = member / 'package.json'
        self.assertEqual((copied / 'package.json').read_text(), source_package.read_text())
        self.assertFalse(os.path.samefile(copied / 'package.json', source_package))
        for dependency, content in (
                ('node_modules/dependency/index.js', 'changed source dependency\n'),
                ('packages/workspace/node_modules/nested/index.js', 'cached nested member dependency\n')):
            self.assertEqual((copied / dependency).read_text(), content)
            self.assertTrue(os.path.samefile(copied / dependency, self.generation / relative / dependency))
            self.assertFalse(os.path.samefile(copied / dependency, member / dependency))
        self.assertEqual((copied / 'packages/late/node_modules/uncached/index.js').read_text(),
                         'not cached\n')
        self.assertTrue(os.path.samefile(
            copied / 'packages/late/node_modules/uncached/index.js',
            self.generation / relative / 'packages/late/node_modules/uncached/index.js'))
        for git in (copied / '.git', copied / 'packages/workspace/.git'):
            self.assertFalse(git.exists())
            self.assertFalse(git.is_symlink())
        self.assertFalse((self.snapshot / unselected).exists())
        for relative_link, target in (
                (Path('repos/member'), copied),
                (Path('node_modules/member'), copied),
                (Path('apps/fractal-web/node_modules/member'), copied),
                (relative / 'workspace', copied / 'packages/workspace'),
                (relative / 'node_modules/workspace', copied / 'packages/workspace')):
            link = self.snapshot / relative_link
            self.assertTrue(link.is_symlink())
            self.assertFalse(os.path.isabs(os.readlink(link)))
            self.assertEqual(link.resolve(), target)
        report = json.loads(self.manifest.read_text())
        self.assertEqual(report['dependencyKey'], self.key)
        self.assertEqual(report['dependencyFiles'], {'linked': 6, 'copied': 0})
        self.assertIn('apps/fractal-web/private/build-live.mjs', report['copiedIgnoredOverlayFiles'])

    def test_external_member_is_rejected_before_copying(self):
        outside = self.root / 'outside'
        outside.mkdir()
        (outside / 'secret').write_text('must not copy\n')
        (self.source / 'repos').mkdir()
        (self.source / 'repos/member').symlink_to(outside)
        self.populate_members({'member': {}})

        with self.assertRaisesRegex(ValueError, 'candidate-local store'):
            self.prepare()

        self.assertFalse((self.snapshot / 'repos').exists())
        self.assertFalse((self.snapshot / 'apps').exists())
        self.assertFalse((self.snapshot / 'node_modules').exists())
        self.assertFalse(self.manifest.exists())

    def test_external_member_store_is_rejected_before_copying(self):
        outside = self.root / 'outside'
        (outside / 'member').mkdir(parents=True)
        (outside / 'member/secret').write_text('must not copy\n')
        (self.source / '.devenv').mkdir()
        (self.source / '.devenv/candidate-members').symlink_to(outside)
        (self.source / 'repos').mkdir()
        (self.source / 'repos/member').symlink_to(self.source / '.devenv/candidate-members/member')
        self.populate_members({'member': {}})

        with self.assertRaisesRegex(ValueError, 'candidate member store'):
            self.prepare()

        self.assertFalse((self.snapshot / '.devenv').exists())
        self.assertFalse((self.snapshot / 'apps').exists())
        self.assertFalse((self.snapshot / 'node_modules').exists())
        self.assertFalse(self.manifest.exists())

    def test_rejects_member_path_escape_before_copying(self):
        self.populate_members({'../outside': {}})
        with self.assertRaisesRegex(ValueError, 'invalid member path'):
            self.prepare()
        self.assertFalse((self.snapshot / 'apps').exists())
        self.assertFalse((self.snapshot / 'node_modules').exists())

    def test_rejects_snapshot_member_store_symlink(self):
        relative = Path('.devenv/candidate-members/member')
        self.write_source(relative / 'package.json', '{"name":"member"}')
        (self.source / 'repos').mkdir()
        (self.source / 'repos/member').symlink_to(self.source / relative)
        self.populate_members({'member': {}})
        outside = self.root / 'outside'
        outside.mkdir()
        (self.snapshot / '.devenv').symlink_to(outside)
        with self.assertRaisesRegex(ValueError, 'destination'):
            self.prepare()
        self.assertEqual(list(outside.iterdir()), [])
        self.assertFalse((self.snapshot / 'node_modules').exists())

if __name__ == '__main__':
    unittest.main(verbosity=2)
