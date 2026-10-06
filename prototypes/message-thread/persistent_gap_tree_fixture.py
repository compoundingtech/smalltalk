"""Invented transactional/reopen checks for persistent_gap_tree.py.

Source fixture only until a fresh model grant permits execution. This does
not exercise real Store admission, projection, checkpoint, or repair.
"""

from __future__ import annotations

import sqlite3
import tempfile
from pathlib import Path

from persistent_gap_tree import PersistentGapTree, SCHEMA


def main() -> None:
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / 'reply-gap.sqlite3'
        db = sqlite3.connect(path)
        db.executescript(SCHEMA)
        db.execute('BEGIN')
        with db:
            tree = PersistentGapTree(db, 'batch/one')
            expected = {}
            for number in range(100):
                index = 2 * number + 3
                gap = 2 * number + 2 - 100_000
                tree.replace_head(child=f'message/far-{number}',
                                  store_index=index, gap=gap,
                                  legacy_claim_id=f'legacy-{number}',
                                  recorded_claim_id=f'recorded-{number}',
                                  legacy_parent='message/other',
                                  recorded_parent='message/root')
                expected[index] = gap
        db.close()

        db = sqlite3.connect(path)
        tree = PersistentGapTree(db, 'batch/one')
        assert tree.gap_at(3) == 2 - 100_000
        original = tree.gap_at(201)
        db.execute('BEGIN')
        assert tree.shift_after(1, -1) == []
        assert tree.gap_at(201) == original - 1
        db.rollback()
        assert tree.gap_at(201) == original
        db.execute('BEGIN')
        with db:
            assert tree.shift_after(1, -1) == []
        expected = {index: gap - 1 for index, gap in expected.items()}
        assert all(tree.gap_at(index) == gap for index, gap in expected.items())
        db.close()

        db = sqlite3.connect(path)
        tree = PersistentGapTree(db, 'batch/one')
        assert tree.gap_at(201) == original - 1
        try:
            tree.replace_head(child='message/far-0', store_index=None)
        except AssertionError as error:
            assert 'writer transaction' in str(error)
        else:
            raise AssertionError('gap replacement outside a transaction succeeded')
        db.execute('BEGIN')
        try:
            tree.replace_head(child='message/far-0', store_index=5, gap=0,
                              legacy_claim_id='invalid-legacy',
                              recorded_claim_id='invalid-recorded')
        except AssertionError:
            pass  # index 5 belongs to another child; OLD leaf must survive
        else:
            raise AssertionError('colliding NEW head replaced the OLD leaf')
        assert tree.gap_at(3) == expected[3]
        db.rollback()
        # Insert and remove inside an existing subtree with a pending lazy
        # shift. The new gap is already at the current cut and must not inherit
        # the earlier shift; every older leaf retains its shifted arithmetic.
        db.execute('BEGIN')
        with db:
            tree.replace_head(child='message/inserted', store_index=300,
                              gap=-5, legacy_claim_id='legacy-inserted',
                              recorded_claim_id='recorded-inserted')
        expected[300] = -5
        assert all(tree.gap_at(index) == gap for index, gap in expected.items())
        db.execute('BEGIN')
        with db:
            tree.replace_head(child='message/inserted', store_index=None)
        expected.pop(300)
        assert all(tree.gap_at(index) == gap for index, gap in expected.items())
        # Direct OLD/NEW lane-head change across batches removes the old leaf
        # and installs a new one at its own current canonical rank gap.
        db.execute('BEGIN')
        other = PersistentGapTree(db, 'batch/two')
        other.replace_head(child='message/far-0', store_index=7, gap=1,
                           legacy_claim_id='legacy-new',
                           recorded_claim_id='recorded-new')
        assert tree.gap_at(3) is None and other.gap_at(7) == 1
        db.rollback()
        assert all(tree.gap_at(index) == gap for index, gap in expected.items())
        assert other.gap_at(7) is None
        db.execute('BEGIN')
        with db:
            other.replace_head(child='message/far-0', store_index=7, gap=1,
                               legacy_claim_id='legacy-new',
                               recorded_claim_id='recorded-new')
        expected.pop(3)
        assert all(tree.gap_at(index) == gap for index, gap in expected.items())
        assert tree.gap_at(3) is None
        assert other.gap_at(7) == 1
        db.execute('BEGIN')
        with db:
            other.replace_head(child='message/far-0', store_index=None)
        assert other.gap_at(7) is None
        assert db.execute(
            "SELECT COUNT(*) FROM reply_gap_leaves WHERE child='message/far-0'"
        ).fetchone()[0] == 0
        db.close()
    print('persistent gap rollback, reopen, cross-batch move and removal passed')


if __name__ == '__main__':
    main()
