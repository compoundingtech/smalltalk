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
        with db:
            tree = PersistentGapTree(db, 'batch/one')
            for number in range(100):
                tree.replace_head(child=f'message/far-{number}',
                                  store_index=2 * number + 3,
                                  gap=2 * number + 2 - 100_000,
                                  legacy_claim_id=f'legacy-{number}',
                                  recorded_claim_id=f'recorded-{number}',
                                  legacy_parent='message/other',
                                  recorded_parent='message/root')
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
        with db:
            assert tree.shift_after(1, -1) == []
        db.close()

        db = sqlite3.connect(path)
        tree = PersistentGapTree(db, 'batch/one')
        assert tree.gap_at(201) == original - 1
        # Direct OLD/NEW lane-head change across batches removes the old leaf
        # and installs a new one at its own current canonical rank gap.
        with db:
            other = PersistentGapTree(db, 'batch/two')
            other.replace_head(child='message/far-0', store_index=7, gap=1,
                               legacy_claim_id='legacy-new',
                               recorded_claim_id='recorded-new')
        assert tree.gap_at(3) is None
        assert other.gap_at(7) == 1
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
