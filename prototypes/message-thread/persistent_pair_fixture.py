"""Queued SQL-coupled leaf-maintenance fixture; not yet executed.

This exercises the invented SQLite schema, not production Store hooks. A
future serialized model unit must execute it and retain raw counters.
"""

from __future__ import annotations

import sqlite3
import tempfile
from pathlib import Path

from mixed_rank_oracle import (SCHEMA as MIXED_SCHEMA, add, exact_parent,
                               lane_parent, rank_bump, refresh, sync_assignment)
from persistent_gap_tree import (SCHEMA as GAP_SCHEMA, PersistentGapTree,
                                 refresh_child_leaf)


def full_fold(db: sqlite3.Connection, children: list[str]) -> None:
    for child in children:
        expected = exact_parent(db, child)
        selected = db.execute(
            'SELECT parent FROM selected_reply WHERE child=?', (child,),
        ).fetchone()
        assert lane_parent(db, child) == expected
        assert (selected[0] if selected else None) == expected


def main() -> None:
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / 'mixed-gap.sqlite3'
        db = sqlite3.connect(path)
        db.executescript(MIXED_SCHEMA + GAP_SCHEMA)
        batch = 'batch/persist-pair/1'
        db.execute("INSERT INTO batches VALUES(?,'host/example',1)", (batch,))
        db.execute("INSERT INTO claims VALUES('predecessor',1,?,'resource/example',"
                   "'resource.observed',NULL,'100')", (batch,))
        rank_bump(db, batch, 1, 1)
        children = []
        for number in range(20):
            child = f'message/persist-{number:05}'
            children.append(child)
            recorded = 100_000 if number < 10 else 2 * number + 1
            add(db, f'z-{number:05}', 2 * number + 2, batch,
                child, 'message/root', recorded=recorded)
            add(db, f'a-{number:05}', 2 * number + 3, batch,
                child, 'message/other')
            refresh_child_leaf(db, child)
        full_fold(db, children)
        db.commit()
        db.close()

        db = sqlite3.connect(path)
        tree = PersistentGapTree(db, batch)
        assert tree.gap_at(3) == 2 - 100_000
        # The next call and all selected-edge refreshes share the claim/rank
        # mutation transaction. The ten far-head children are pruned.
        with db:
            candidates = tree.shift_after(1, -1)
            assert candidates == children[10:]
            db.execute("DELETE FROM claims WHERE id='predecessor'")
            rank_bump(db, batch, 1, -1)
            for child in candidates:
                refresh(db, child)
        full_fold(db, children)
        assert tree.gap_at(3) == 1 - 100_000
        db.close()

        db = sqlite3.connect(path)
        tree = PersistentGapTree(db, batch)
        assert tree.gap_at(3) == 1 - 100_000
        # A record-position change names its direct child, so recompute its
        # two lane heads; rollback must restore leaf, assignment and selected
        # parent together.
        db.execute('BEGIN')
        db.execute("UPDATE replica_records SET position=0 "
                   "WHERE claim_id='z-00000'")
        sync_assignment(db, 'z-00000')
        refresh_child_leaf(db, children[0])
        refresh(db, children[0])
        full_fold(db, children)
        db.rollback()
        assert tree.gap_at(3) == 1 - 100_000
        full_fold(db, children)
        with db:
            db.execute("UPDATE replica_records SET position=0 "
                       "WHERE claim_id='z-00000'")
            sync_assignment(db, 'z-00000')
            refresh_child_leaf(db, children[0])
            refresh(db, children[0])
        full_fold(db, children)
        db.close()
    print('persistent selected lane parity, rollback and reopen passed')


if __name__ == '__main__':
    main()
