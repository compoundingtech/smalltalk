#!/usr/bin/env python3
"""Tests for the gq prototype. Run by hand: python3 tests/git-queue/test_gq.py

Each test builds scratch repositories in a temp directory: a bare `origin` (stands in for GitHub
or any git storage) and clones that stand in for separate machines. Nothing touches the network.
"""
import os
import subprocess
import sys
import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

GQ = str(Path(__file__).resolve().parent / 'gq')
ENV = dict(os.environ, GIT_CONFIG_GLOBAL='/dev/null', GIT_CONFIG_SYSTEM='/dev/null',
           GIT_AUTHOR_NAME='dev', GIT_AUTHOR_EMAIL='dev@example.com',
           GIT_COMMITTER_NAME='dev', GIT_COMMITTER_EMAIL='dev@example.com')
ENV.pop('GQ_NOW', None)


def git(path, *args, check=True, env=None):
    p = subprocess.run(['git', '-C', str(path), *args], text=True, capture_output=True,
                       env=dict(ENV, **(env or {})))
    if check and p.returncode != 0:
        raise AssertionError(f'git {args}: {p.stderr}')
    return p.stdout.strip()


def gq(repo, *args, who='m', env=None, **kw):
    """Run gq as a machine. Returns (exit code, stdout)."""
    p = subprocess.run([sys.executable, GQ, '--repo', str(repo), '--who', who, *args], text=True,
                       capture_output=True, env=dict(ENV, **(env or {})), **kw)
    return p.returncode, p.stdout.strip() + p.stderr.strip()


class World:
    def __init__(self, root):
        self.root = Path(root)
        self.origin = self.root / 'origin.git'
        subprocess.run(['git', 'init', '-q', '--bare', '-b', 'main', str(self.origin)], check=True, env=ENV)
        seed = self.clone('seed')
        self.commit(seed, 'README', 'hello\n', 'initial')
        git(seed, 'push', '-q', 'origin', 'main')

    def clone(self, name, remote=None):
        path = self.root / name
        subprocess.run(['git', 'clone', '-q', str(remote or self.origin), str(path)], check=True,
                       env=ENV, capture_output=True)
        return path

    def commit(self, repo, name, text, msg, parent=None):
        if parent:
            git(repo, 'checkout', '-q', '--detach', parent)
        (Path(repo) / name).write_text(text)
        git(repo, 'add', name)
        git(repo, 'commit', '-q', '-m', msg)
        return git(repo, 'rev-parse', 'HEAD')

    def main(self):
        return git(self.origin, 'rev-parse', 'refs/heads/main')

    def ref(self, name):
        return subprocess.run(['git', '-C', str(self.origin), 'rev-parse', '--verify', '-q', name],
                              text=True, capture_output=True).stdout.strip()

    def files_on_main(self):
        return sorted(git(self.origin, 'ls-tree', '--name-only', 'main').split())


class QueueTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.w = World(self.tmp.name)
        self.a = self.w.clone('a')      # machine A
        self.b = self.w.clone('b')      # machine B

    def publish(self, repo, name, text='x\n', form='commit', parent=None, publisher='dev'):
        c = self.w.commit(repo, name, text, f'add {name}', parent)
        args = ['publish', c] + (['--form', 'range'] if form == 'range' else [])
        if form == 'range':
            args = ['publish', c, git(repo, 'rev-parse', c + '^'), '--form', 'range']
        code, out = gq(repo, *args, '--publisher', publisher)
        self.assertEqual(code, 0, out)
        return c, git(repo, 'rev-parse', c + '^{tree}')

    # -- git behaviour the design relies on ------------------------------------------------
    def test_blob_ref_accepted_under_queue_but_not_under_heads(self):
        blob = subprocess.run(
            ['git', '-C', str(self.a), 'hash-object', '-w', '--stdin'], input='a..b\n', text=True,
            capture_output=True, env=ENV).stdout.strip()
        git(self.a, 'push', '-q', 'origin', f'{blob}:refs/queue/ready/t/p')
        self.assertEqual(self.w.ref('refs/queue/ready/t/p'), blob)
        bad = subprocess.run(['git', '-C', str(self.a), 'push', 'origin', f'{blob}:refs/heads/x'],
                             text=True, capture_output=True, env=ENV)
        self.assertNotEqual(bad.returncode, 0)

    # -- the whole path --------------------------------------------------------------------
    def test_publish_list_claim_land_commit_form(self):
        c, tree = self.publish(self.a, 'one.txt')
        code, out = gq(self.b, 'ls')
        self.assertEqual(code, 0)
        self.assertRegex(out, rf'^ready\s+{tree[:12]}')
        code, out = gq(self.b, 'land', tree)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.w.main(), c)            # a fast-forward: exactly the advertised commit
        self.assertIn('one.txt', self.w.files_on_main())
        self.assertTrue(self.w.ref(f'refs/queue/landed/{tree}'))
        self.assertFalse(self.w.ref(f'refs/queue/claim/{tree}'))
        self.assertFalse(self.w.ref(f'refs/queue/ready/{tree}/dev'))
        self.assertRegex(gq(self.b, 'ls')[1] or 'empty', r'^empty$')

    def test_range_form_ref_points_at_blob(self):
        c, tree = self.publish(self.a, 'range.txt', form='range')
        self.assertEqual(git(self.w.origin, 'cat-file', '-t', f'refs/queue/ready/{tree}/dev'), 'blob')
        self.assertRegex(git(self.w.origin, 'cat-file', 'blob', f'refs/queue/ready/{tree}/dev'),
                         r'^[0-9a-f]{40}\.\.' + c)
        code, out = gq(self.b, 'land', tree)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.w.main(), c)

    def test_run_lands_in_order_and_replays_when_main_moved(self):
        base = git(self.a, 'rev-parse', 'HEAD')
        c1, t1 = self.publish(self.a, 'one.txt', parent=base)
        c2, t2 = self.publish(self.a, 'two.txt', parent=base)       # both branch from the same base
        code, out = gq(self.b, 'run', '--once')
        self.assertEqual(code, 0, out)
        self.assertIn('done: landed 2', out)
        self.assertEqual(self.w.files_on_main(), ['README', 'one.txt', 'two.txt'])
        self.assertEqual(git(self.w.origin, 'log', '--format=%s', 'main').split('\n')[:2],
                         ['add two.txt', 'add one.txt'])            # published first, landed first
        self.assertNotEqual(self.w.main(), c2)                      # the second was replayed onto the first

    def test_replay_conflict_is_refused_not_landed(self):
        base = git(self.a, 'rev-parse', 'HEAD')
        c1, t1 = self.publish(self.a, 'same.txt', 'one\n', parent=base)
        c2, t2 = self.publish(self.a, 'same.txt', 'two\n', parent=base)
        self.assertEqual(gq(self.b, 'land', t1)[0], 0)
        code, out = gq(self.b, 'land', t2)
        self.assertEqual(code, 11, out)
        self.assertIn('conflict', out)
        self.assertFalse(self.w.ref(f'refs/queue/claim/{t2}'))       # claim released

    # -- racing ----------------------------------------------------------------------------
    def test_many_concurrent_claims_exactly_one_winner(self):
        c, tree = self.publish(self.a, 'race.txt')
        machines = [self.w.clone(f'm{i}') for i in range(8)]
        with ThreadPoolExecutor(8) as pool:
            results = list(pool.map(lambda i: gq(machines[i], 'claim', tree, who=f'm{i}'), range(8)))
        winners = [r for r in results if r[0] == 0]
        self.assertEqual(len(winners), 1, results)
        self.assertTrue(all(r[0] == 10 for r in results if r[0] != 0), results)
        claim = git(self.w.origin, 'cat-file', 'blob', f'refs/queue/claim/{tree}')
        self.assertIn('who=m', claim)

    def test_two_clones_race_to_land_one_bundle_exactly_one_wins(self):
        c, tree = self.publish(self.a, 'race.txt')
        with ThreadPoolExecutor(2) as pool:
            results = list(pool.map(lambda m: gq(m, 'land', tree, who=m.name), [self.a, self.b]))
        codes = sorted(r[0] for r in results)
        self.assertIn(codes, ([0, 10], [0, 12]), results)            # one lands; the other lost or saw it landed
        self.assertEqual(self.w.main(), c)
        self.assertEqual(git(self.w.origin, 'rev-list', '--count', 'main'), '2')

    def test_concurrent_run_loops_land_everything_once(self):
        base = git(self.a, 'rev-parse', 'HEAD')
        trees = [self.publish(self.a, f'f{i}.txt', parent=base)[1] for i in range(4)]
        with ThreadPoolExecutor(2) as pool:
            results = list(pool.map(lambda m: gq(m, 'run', '--once', who=m.name), [self.a, self.b]))
        self.assertTrue(all(r[0] == 0 for r in results), results)
        self.assertEqual(self.w.files_on_main(), ['README'] + [f'f{i}.txt' for i in range(4)])
        for t in trees:
            self.assertTrue(self.w.ref(f'refs/queue/landed/{t}'))

    # -- failure modes ---------------------------------------------------------------------
    def test_stale_advertisement_already_landed_by_someone_else(self):
        c, tree = self.publish(self.a, 'stale.txt')
        gq(self.b, 'ls')                                           # B mirrors and now holds a stale view
        self.assertEqual(gq(self.a, 'land', tree)[0], 0)            # A lands it
        code, out = gq(self.b, 'land', tree)                        # B acts on what it saw
        self.assertEqual(code, 12, out)
        self.assertIn('already landed', out)
        self.assertFalse(self.w.ref(f'refs/queue/claim/{tree}'))
        self.assertEqual(git(self.w.origin, 'rev-list', '--count', 'main'), '2')

    def test_a_landed_marker_counts_whatever_its_contents(self):
        c, tree = self.publish(self.a, 'tomb.txt')
        blob = subprocess.run(['git', '-C', str(self.a), 'hash-object', '-w', '--stdin'],
                              input='landed\n', text=True, capture_output=True, env=ENV).stdout.strip()
        git(self.a, 'push', '-q', 'origin', f'{blob}:refs/queue/landed/{tree}')
        self.assertRegex(gq(self.b, 'ls')[1], r'^landed\s')
        code, out = gq(self.b, 'land', tree)
        self.assertEqual(code, 12, out)
        self.assertNotIn('tomb.txt', self.w.files_on_main())

    def test_stale_advertisement_withdrawn_before_claim(self):
        c, tree = self.publish(self.a, 'gone.txt')
        gq(self.b, 'ls')
        git(self.a, 'push', '-q', 'origin', f':refs/queue/ready/{tree}/dev')
        code, out = gq(self.b, 'land', tree)
        self.assertEqual(code, 11, out)
        self.assertIn('no bundle advertises', out)
        self.assertEqual(self.w.main(), git(self.a, 'rev-parse', 'origin/main'))

    def test_content_changed_after_advertised_is_refused(self):
        c, tree = self.publish(self.a, 'swap.txt', 'good\n')
        evil = self.w.commit(self.a, 'swap.txt', 'EVIL\n', 'looks the same', parent=git(self.a, 'rev-parse', c + '^'))
        git(self.a, 'push', '-q', 'origin', f'+{evil}:refs/queue/ready/{tree}/dev')   # same name, other content
        code, out = gq(self.b, 'ls')
        self.assertRegex(out, r'^changed\s', out)
        code, out = gq(self.b, 'land', tree)
        self.assertEqual(code, 11, out)
        self.assertIn('changed', out)
        self.assertNotIn('swap.txt', self.w.files_on_main())
        self.assertFalse(self.w.ref(f'refs/queue/claim/{tree}'))

    def test_content_changed_in_range_blob_is_refused(self):
        c, tree = self.publish(self.a, 'swap2.txt', 'good\n', form='range')
        evil = self.w.commit(self.a, 'swap2.txt', 'EVIL\n', 'x', parent=git(self.a, 'rev-parse', c + '^'))
        blob = subprocess.run(['git', '-C', str(self.a), 'hash-object', '-w', '--stdin'],
                              input=f'{evil}\n', text=True, capture_output=True, env=ENV).stdout.strip()
        git(self.a, 'push', '-q', 'origin', f'+{evil}:refs/queue/keep/{tree}/dev', f'+{blob}:refs/queue/ready/{tree}/dev')
        code, out = gq(self.b, 'land', tree)
        self.assertEqual(code, 11, out)
        self.assertNotIn('swap2.txt', self.w.files_on_main())

    def test_another_publisher_of_the_same_tree_is_used_when_one_is_bad(self):
        c, tree = self.publish(self.a, 'dup.txt', 'good\n', publisher='alice')
        git(self.a, 'push', '-q', 'origin', f'+{c}:refs/queue/ready/{tree}/bob')
        evil = self.w.commit(self.a, 'dup.txt', 'EVIL\n', 'x', parent=git(self.a, 'rev-parse', c + '^'))
        git(self.a, 'push', '-q', 'origin', f'+{evil}:refs/queue/ready/{tree}/alice')
        code, out = gq(self.b, 'land', tree)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.w.main(), c)

    def test_machine_disappears_mid_claim_and_the_claim_expires(self):
        c, tree = self.publish(self.a, 'dead.txt')
        t0 = 1_000_000
        self.assertEqual(gq(self.a, '--ttl', '60', 'claim', tree, who='dies', env={'GQ_NOW': str(t0)})[0], 0)
        # Before expiry nobody else can take it.
        code, out = gq(self.b, 'land', tree, who='other', env={'GQ_NOW': str(t0 + 30)})
        self.assertEqual(code, 10, out)
        self.assertEqual(self.w.main(), git(self.a, 'rev-parse', 'origin/main'))
        # After expiry another machine takes over with a compare-and-swap and lands it.
        code, out = gq(self.b, 'land', tree, who='other', env={'GQ_NOW': str(t0 + 61)})
        self.assertEqual(code, 0, out)
        self.assertEqual(self.w.main(), c)

    def test_a_late_machine_cannot_land_on_an_expired_and_replaced_claim(self):
        # The machine that "disappeared" comes back and tries to push its landing with its old claim.
        c, tree = self.publish(self.a, 'late.txt')
        t0 = 1_000_000
        gq(self.a, '--ttl', '60', 'claim', tree, who='slow', env={'GQ_NOW': str(t0)})
        old = self.w.ref(f'refs/queue/claim/{tree}')
        gq(self.b, '--ttl', '600', 'claim', tree, who='taker', env={'GQ_NOW': str(t0 + 100)})
        taker = self.w.ref(f'refs/queue/claim/{tree}')
        self.assertNotEqual(old, taker)
        # The slow machine's atomic landing push leases the claim at its old value: it must be refused.
        landed = subprocess.run(['git', '-C', str(self.a), 'hash-object', '-w', '--stdin'],
                                input='x\n', text=True, capture_output=True, env=ENV).stdout.strip()
        p = subprocess.run(['git', '-C', str(self.a), 'push', '--atomic', 'origin',
                            f'--force-with-lease=refs/queue/claim/{tree}:{old}', f'{c}:refs/heads/main',
                            f':refs/queue/claim/{tree}', f'{landed}:refs/queue/landed/{tree}'],
                           text=True, capture_output=True, env=ENV)
        self.assertNotEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(self.w.main(), git(self.b, 'rev-parse', 'origin/main'))
        self.assertFalse(self.w.ref(f'refs/queue/landed/{tree}'))

    def test_claims_by_one_machine_twice_have_different_values(self):
        c, tree = self.publish(self.a, 'nonce.txt')
        gq(self.a, '--ttl', '1', 'claim', tree, who='same', env={'GQ_NOW': '100'})
        first = self.w.ref(f'refs/queue/claim/{tree}')
        gq(self.a, '--ttl', '1', 'claim', tree, who='same', env={'GQ_NOW': '200'})
        self.assertNotEqual(first, self.w.ref(f'refs/queue/claim/{tree}'))

    # -- pulling from more than one place --------------------------------------------------
    def test_pulls_a_bundle_straight_from_a_peers_machine(self):
        peer = self.w.clone('peer')                                 # Nathan's machine: no GitHub involved
        c = self.w.commit(peer, 'p.txt', 'peer\n', 'from the peer machine')
        tree = git(peer, 'rev-parse', c + '^{tree}')
        git(peer, 'update-ref', f'refs/queue/ready/{tree}/nathan', c)   # advertise locally, no push anywhere
        git(self.b, 'remote', 'add', 'peer', str(peer))
        code, out = gq(self.b, '--source', 'peer', 'ls')
        self.assertRegex(out, rf'^ready\s+{tree[:12]}.*peer', out)
        code, out = gq(self.b, '--source', 'peer', 'land', tree)
        self.assertEqual(code, 0, out)
        self.assertEqual(self.w.main(), c)                          # objects came from the peer, main is on origin

    # -- evidence and checks ---------------------------------------------------------------
    def test_check_failure_refuses_and_releases_the_claim(self):
        c, tree = self.publish(self.a, 'bad.txt')
        code, out = gq(self.b, '--check', 'false', 'land', tree)
        self.assertEqual(code, 11, out)
        self.assertIn('check failed', out)
        self.assertFalse(self.w.ref(f'refs/queue/claim/{tree}'))

    def test_a_trusted_note_over_the_exact_tree_skips_the_check(self):
        c, tree = self.publish(self.a, 'ok.txt')
        covered = Path(self.tmp.name) / 'covered'
        covered.write_text(tree + '\n')
        cover = Path(self.tmp.name) / 'cover.sh'
        cover.write_text(f'#!/bin/sh\ngrep -qx "$1" {covered}\n')
        cover.chmod(0o755)
        code, out = gq(self.b, '--check', 'false', '--evidence-cmd', str(cover), 'land', tree)
        self.assertEqual(code, 0, out)                              # `false` was never run
        # When main has moved the landing tree is a new tree; the old note no longer covers it.
        base = git(self.a, 'rev-parse', 'HEAD~1')
        c2, t2 = self.publish(self.a, 'other.txt', parent=base)
        covered.write_text(t2 + '\n')
        code, out = gq(self.b, '--check', 'false', '--evidence-cmd', str(cover), 'land', t2)
        self.assertEqual(code, 11, out)
        self.assertIn('check failed', out)


if __name__ == '__main__':
    unittest.main(verbosity=2)
