#!/usr/bin/env python3
"""Release safety regressions against real commit trees, with no network or builds."""
import copy
import contextlib
import importlib.machinery
import importlib.util
import json
import io
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
from release_notes import FIELDS, ImpactError, check_pull_request, collect, render, validate

ROOT = Path(__file__).resolve().parent.parent
loader = importlib.machinery.SourceFileLoader('release_daily', str(ROOT / 'scripts/release-smalltalk-daily'))
spec = importlib.util.spec_from_loader(loader.name, loader)
daily = importlib.util.module_from_spec(spec)
loader.exec_module(daily)


def fragment(summary='Example change'):
    return {'version': 1, 'summary': summary, **{
        field: {'status': 'none', 'detail': 'No additional upgrade effect.'} for field in FIELDS
    }}


class ReleaseNotes(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name)
        self.git('init', '-q')
        self.git('config', 'user.name', 'Example')
        self.git('config', 'user.email', 'example@example.invalid')
        self.git('config', 'commit.gpgsign', 'false')
        self.git('config', 'core.hooksPath', '/dev/null')
        self.write('crates/smallclaims/src/store.rs', 'pub const SCHEMA_VERSION: &str = "PRAGMA user_version = 16;";')
        self.write('crates/st3/src/store/checkpoint_rules.rs', 'pub const RULES_VERSION: u32 = 10;')
        self.write('docs/st3/binary-releases.md', '# Pinned install guide\n')
        self.base = self.commit('Base')
        self.git('tag', 'v0.3.0')

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.repo, text=True, stderr=subprocess.PIPE).strip()

    def write(self, name, content):
        path = self.repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def add_fragment(self, name='change', value=None):
        self.write(f'release-notes/{name}.json', json.dumps(value or fragment()))

    def commit(self, message):
        self.git('add', '.')
        self.git('commit', '-qm', message)
        return self.git('rev-parse', 'HEAD')

    def test_missing_fields_and_ambiguous_none_are_rejected(self):
        value = fragment()
        del value['manual']
        with self.assertRaisesRegex(ImpactError, 'missing required fields'):
            validate(value, 'example.json')
        value = fragment()
        value['rules']['transitions'] = [[10, 11]]
        with self.assertRaisesRegex(ImpactError, 'none cannot declare'):
            validate(value, 'example.json')
        value = fragment()
        value['replay'] = {'status': 'unknown', 'detail': ''}
        with self.assertRaisesRegex(ImpactError, 'nonempty'):
            validate(value, 'example.json')

    def test_direct_unclassified_commit_holds_publication(self):
        self.write('fix.rs', '// Direct fix\n')
        missing = self.commit('Direct commit without classification')
        self.add_fragment('later')
        source = self.commit('Classified later change')
        with self.assertRaisesRegex(ImpactError, missing):
            render(self.repo, 'v0.3.0', source, 'v0.3.1')

    def test_pr_requires_fresh_fragment_even_for_documentation(self):
        self.write('guide.md', '# Documentation only\n')
        source = self.commit('Docs without upgrade note')
        with self.assertRaisesRegex(ImpactError, 'add a uniquely named'):
            check_pull_request(self.repo, self.base, source)

    def test_pr_does_not_inherit_unreleased_main_backlog(self):
        self.write('older.rs', '// Unclassified main change\n')
        base = self.commit('Older main change')
        self.git('checkout', '-qb', 'feature')
        self.write('feature.rs', '// Multi-commit PR\n')
        self.commit('Feature implementation')
        self.add_fragment('feature')
        self.commit('Feature note')
        self.git('checkout', '-q', '-')
        self.git('merge', '--no-ff', '-qm', 'Effective PR merge', 'feature')
        source = self.git('rev-parse', 'HEAD')
        self.assertEqual(list(check_pull_request(self.repo, base, source)['coverage']), [source])
        with self.assertRaisesRegex(ImpactError, base):
            collect(self.repo, self.base, source)
        result = subprocess.run([sys.executable, str(ROOT / 'scripts/check-release-impact'),
                                 '--repo', str(self.repo), '--base', base, '--source', source],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(source, result.stdout)

    def test_queue_requires_a_fragment_from_every_integrated_pr(self):
        self.git('checkout', '-qb', 'one')
        self.write('one.rs', '// No note\n')
        self.commit('First implementation')
        self.git('checkout', '-q', '-')
        self.git('merge', '--no-ff', '-qm', 'Merge first PR', 'one')
        missing = self.git('rev-parse', 'HEAD')
        self.git('checkout', '-qb', 'two')
        value = fragment()
        value['commits'] = [missing]  # A later backfill cannot bypass PR authorship.
        self.add_fragment('two', value)
        self.commit('Second PR with historical backfill')
        self.git('checkout', '-q', '-')
        self.git('merge', '--no-ff', '-qm', 'Merge second PR', 'two')
        with self.assertRaisesRegex(ImpactError, missing):
            check_pull_request(self.repo, self.base, 'HEAD')

    def test_pr_cannot_edit_delete_or_rename_existing_metadata(self):
        self.add_fragment('published')
        base = self.commit('Published note')
        for operation in ('edit', 'delete', 'rename'):
            self.git('reset', '--hard', base)
            if operation == 'edit':
                self.add_fragment('published', fragment('Changed old summary'))
            elif operation == 'delete':
                self.git('rm', 'release-notes/published.json')
            else:
                self.git('mv', 'release-notes/published.json', 'release-notes/renamed.json')
            self.add_fragment('fresh')
            source = self.commit(operation)
            with self.assertRaisesRegex(ImpactError, 'existing fragments are immutable'):
                check_pull_request(self.repo, base, source)

    def test_pr_rejects_invalid_metadata_and_undeclared_version_change(self):
        self.write('release-notes/bad.json', '{')
        source = self.commit('Invalid fragment')
        with self.assertRaisesRegex(ImpactError, 'invalid JSON'):
            check_pull_request(self.repo, self.base, source)
        self.git('reset', '--hard', self.base)
        self.write('crates/st3/src/store/checkpoint_rules.rs', 'pub const RULES_VERSION: u32 = 11;')
        self.add_fragment()
        source = self.commit('Missing rules transition')
        with self.assertRaisesRegex(ImpactError, 'rules version 10 -> 11'):
            check_pull_request(self.repo, self.base, source)

    def test_pr_accepts_explicit_unknown_and_matching_version_transitions(self):
        self.write('crates/smallclaims/src/store.rs', 'pub const SCHEMA_VERSION: &str = "PRAGMA user_version = 17;";')
        self.write('crates/st3/src/store/checkpoint_rules.rs', 'pub const RULES_VERSION: u32 = 11;')
        value = fragment()
        value['replay'] = {'status': 'unknown', 'detail': 'Replay on populated state has not been measured; wait for API readiness.'}
        value['schema'] = {'status': 'changed', 'detail': 'Forward migration; preserve backup.', 'transitions': [[16, 17]]}
        value['rules'] = {'status': 'changed', 'detail': 'Coordinate members.', 'transitions': [[10, 11]]}
        self.add_fragment(value=value)
        source = self.commit('Classified contract change')
        self.assertEqual(len(check_pull_request(self.repo, self.base, source)['version_changes']), 2)

    def test_explicit_source_backfill_and_source_pinning(self):
        self.write('fix.rs', '// Direct fix\n')
        missing = self.commit('Direct change')
        value = fragment()
        value['commits'] = [missing]
        self.add_fragment(value=value)
        source = self.commit('Source-pinned backfill')
        expected, report = render(self.repo, 'v0.3.0', source, 'v0.3.1')
        self.assertEqual(set(report['coverage']), {missing, source})
        self.write('docs/st3/binary-releases.md', 'Uncommitted wrong guide')
        self.add_fragment(value={'bad': 'uncommitted metadata'})
        self.assertEqual(render(self.repo, 'v0.3.0', source, 'v0.3.1')[0], expected)
        self.assertIn('Pinned install guide', expected)
        self.assertNotIn('Uncommitted wrong guide', expected)

    def test_rules_change_needs_matching_classification(self):
        self.write('crates/st3/src/store/checkpoint_rules.rs', 'pub const RULES_VERSION: u32 = 11;')
        self.add_fragment()
        source = self.commit('Unclassified rules change')
        with self.assertRaisesRegex(ImpactError, 'rules version 10 -> 11'):
            collect(self.repo, self.base, source)

    def test_intermediate_schema_and_rules_changes_are_preserved(self):
        for schema, rules, name, transitions in [(17, 11, 'first', [16, 17, 10, 11]), (16, 12, 'second', [17, 16, 11, 12])]:
            self.write('crates/smallclaims/src/store.rs', f'pub const SCHEMA_VERSION: &str = "PRAGMA user_version = {schema};";')
            self.write('crates/st3/src/store/checkpoint_rules.rs', f'pub const RULES_VERSION: u32 = {rules};')
            value = fragment(name)
            value['schema'] = {'status': 'changed', 'detail': f'Schema migration {name}; roll forward.', 'transitions': [transitions[:2]]}
            value['rules'] = {'status': 'changed', 'detail': 'Coordinate all members.', 'transitions': [transitions[2:]]}
            self.add_fragment(name, value)
            source = self.commit(name)
        notes, report = render(self.repo, 'v0.3.0', source, 'v0.3.1')
        self.assertEqual(len(report['version_changes']), 4)
        self.assertEqual(report['versions']['schema'], [16, 16])
        self.assertIn('16 → 17', notes)
        self.assertIn('17 → 16', notes)
        self.assertIn('10 → 11', notes)
        self.assertIn('11 → 12', notes)

    def test_merge_fragment_covers_integrated_change(self):
        self.git('checkout', '-qb', 'feature')
        self.write('feature.rs', '// A branch implementation\n')
        self.commit('Implementation')
        self.add_fragment('feature')
        self.commit('Metadata')
        self.git('checkout', '-q', '-')
        self.git('merge', '--no-ff', '-qm', 'Merge pull request #42 from example/feature\n\nAdd a feature', 'feature')
        source = self.git('rev-parse', 'HEAD')
        notes, report = render(self.repo, 'v0.3.0', source, 'v0.3.1')
        self.assertEqual(list(report['coverage']), [source])
        self.assertIn('[Add a feature](https://github.com/compoundingtech/smalltalk/pull/42)', notes)

    def test_unknown_conditional_replay_manual_steps_and_observations(self):
        value = fragment()
        value['replay'] = {'status': 'unknown', 'detail': 'Full replay observed on stale populated state; exact trigger unresolved.'}
        value['manual'] = {'status': 'changed', 'detail': 'Re-pair existing devices and repair saved imports; follow the pinned guide.'}
        value['downtime'] = {'status': 'unknown', 'detail': 'Other graphs have not been measured.', 'observations': [{
            'source': self.base, 'platform': 'Linux x86_64', 'graph_size': '50000 claims',
            'method': 'Restart timestamp to HTTP 200', 'api_seconds': 501, 'health_seconds': None,
        }]}
        self.add_fragment(value=value)
        source = self.commit('Conditional replay guidance')
        notes, _ = render(self.repo, 'v0.3.0', source, 'v0.3.1')
        self.assertIn('**Unknown:** Full replay observed', notes)
        self.assertIn('Re-pair existing devices', notes)
        self.assertIn('restart-to-API 501 seconds; restart-to-health unmeasured', notes)
        self.assertIn('not a downtime guarantee', notes)
        bad = copy.deepcopy(value)
        bad['downtime']['observations'][0]['api_seconds'] = float('inf')
        with self.assertRaisesRegex(ImpactError, 'finite nonnegative'):
            validate(bad, 'bad.json')

    def test_removed_metadata_requires_explicit_historical_backfill(self):
        self.add_fragment('old')
        initial = self.commit('Initial metadata')
        self.git('mv', 'release-notes/old.json', 'release-notes/new.json')
        renamed = self.commit('Rename classification')
        with self.assertRaisesRegex(ImpactError, initial):
            collect(self.repo, self.base, renamed)
        value = fragment()
        value['commits'] = [initial]
        self.add_fragment('new', value)
        source = self.commit('Backfill renamed metadata')
        self.assertEqual(len(collect(self.repo, self.base, source)['coverage']), 3)

    def test_daily_manual_and_tag_render_the_same_bytes(self):
        self.add_fragment()
        source = self.commit('Classified change')
        expected, report = render(self.repo, 'v0.3.0', source, 'v0.3.1')
        notes = self.repo / 'notes.txt'
        manifest = self.repo / 'impact.json'
        subprocess.run([sys.executable, str(ROOT / 'scripts/release_notes.py'), '--repo', str(self.repo),
                        '--previous', 'v0.3.0', '--source', source, '--tag', 'v0.3.1',
                        '--output', str(notes), '--manifest', str(manifest)], check=True)
        self.assertEqual(notes.read_text(), expected)
        self.assertEqual(json.loads(manifest.read_text()), report)
        # Execute the daily dry run too, replacing only external artifact acquisition/proof.
        # The real candidate-tree classifier and renderer still run.
        downloads = self.repo / 'downloads'
        downloads.mkdir()
        commands = []
        def run(*args, cwd=None):
            commands.append(args)
            if args[0] == 'git':
                return self.git(*args[1:])
            if args[:3] == ('gh', 'run', 'download'):
                (downloads / 'example.tar.gz').write_bytes(b'archive fixture')
                (downloads / 'example.tar.gz.sha256').write_text('checksum fixture')
                (downloads / 'SHA256SUMS').write_text('checksum fixture')
                (downloads / 'RELEASE.json').write_text(json.dumps({'source': source}))
                return ''
            if args[0] in ('sha256sum', 'python3'):
                return 'Existing artifact proof fixture'
            raise AssertionError(f'Unexpected publication command in dry run: {args}')
        output = io.StringIO()
        with patch.object(sys, 'argv', ['release-smalltalk-daily', '--dry-run']), \
             patch.object(daily, '__file__', str(self.repo / 'scripts/release-smalltalk-daily')), \
             patch.object(daily, 'newest_verified_main_run', return_value=(1, source)), \
             patch.object(daily, 'latest_release_tag', return_value='v0.3.0'), \
             patch.object(daily, 'next_tag', return_value='v0.3.1'), \
             patch.object(daily, 'run', side_effect=run), \
             patch.object(daily.tempfile, 'mkdtemp', return_value=str(downloads)), \
             contextlib.redirect_stdout(output):
            self.assertEqual(daily.main(), 0)
        self.assertIn(expected, output.getvalue())
        self.assertEqual((downloads / 'RELEASE-NOTES.md').read_text(), expected)
        self.assertEqual(json.loads((downloads / 'UPGRADE-IMPACT.json').read_text()), report)
        self.assertFalse(any(args[:2] == ('gh', 'release') for args in commands))
        workflow = (ROOT / '.github/workflows/release-smalltalk.yml.genie.ts').read_text()
        self.assertIn('python3 scripts/release_notes.py --source', workflow)
        self.assertLess(workflow.index('python3 scripts/release_notes.py --source'), workflow.index('gh release create'))

    def test_daily_rejects_missing_classification_before_download_or_release(self):
        self.write('unclassified.rs', '// change')
        source = self.commit('Unclassified')
        commands = []
        def run(*args, **kwargs):
            commands.append(args)
            if args[:2] == ('git', 'rev-parse'):
                return self.base
            raise AssertionError(f'Unexpected command before classification: {args}')
        with patch.object(sys, 'argv', ['release-smalltalk-daily']), \
             patch.object(daily, 'newest_verified_main_run', return_value=(1, source)), \
             patch.object(daily, 'latest_release_tag', return_value='v0.3.0'), \
             patch.object(daily, 'next_tag', return_value='v0.3.1'), \
             patch.object(daily, 'run', side_effect=run), \
             patch.object(daily.subprocess, 'run', side_effect=[subprocess.CompletedProcess([], 1), subprocess.CompletedProcess([], 1)]), \
             patch.object(daily, 'render', side_effect=ImpactError('Missing upgrade classifications')):
            self.assertEqual(daily.main(), 1)
        self.assertFalse(any(args[0] == 'gh' for args in commands))


if __name__ == '__main__':
    unittest.main()
