"""Queued two-scale SQL writer-cost driver for persisted rank gaps.

DO NOT treat source or parser checks as an executed cost receipt. Running this
large fixture needs a fresh serialized model grant and whole-unit cap.
"""

from __future__ import annotations

import sqlite3
import sys

from mixed_rank_oracle import (SCHEMA as MIXED_SCHEMA, add, exact_parent,
                               measured, rank_bump, refresh)
from persistent_gap_tree import (SCHEMA as GAP_SCHEMA, PersistentGapTree,
                                 refresh_child_leaf)


def case(size: int, mode: str) -> dict:
    db = sqlite3.connect(':memory:')
    db.executescript(MIXED_SCHEMA + GAP_SCHEMA)
    target = 'batch/persist-cost/target'
    db.execute("INSERT INTO batches VALUES(?,'host/example',1)", (target,))
    db.execute("INSERT INTO claims VALUES('predecessor',1,?,'resource/example',"
               "'resource.observed',NULL,'100')", (target,))
    rank_bump(db, target, 1, 1)
    affected = 2 if mode == 'fixed-unrelated' else size
    recorded_far = mode == 'far-same-answer'
    children = []
    for number in range(affected):
        child = f'message/cost-{number:05}'
        children.append(child)
        recorded_position = 100_000 if recorded_far else 2 * number + 1
        add(db, f'z-{number:05}', 2 * number + 2, target,
            child, 'message/root', recorded=recorded_position)
        add(db, f'a-{number:05}', 2 * number + 3, target,
            child, 'message/other')
        refresh_child_leaf(db, child)
    if mode == 'fixed-unrelated':
        unrelated = 'batch/persist-cost/unrelated'
        db.execute("INSERT INTO batches VALUES(?,'host/example',2)", (unrelated,))
        for number in range(size):
            index = 100 + number
            db.execute("INSERT INTO claims VALUES(?,?,?,'resource/unrelated',"
                       "'resource.observed',NULL,'100')",
                       (f'unrelated-{number}', index, unrelated))
            rank_bump(db, unrelated, index, 1)
    db.commit()
    tree = PersistentGapTree(db, target)

    def writer() -> list[str]:
        db.execute('SAVEPOINT selected_reply_writer')
        candidates = tree.shift_after(1, -1)
        db.execute("DELETE FROM claims WHERE id='predecessor'")
        rank_bump(db, target, 1, -1)
        for child in candidates:
            refresh(db, child)
        db.execute('RELEASE selected_reply_writer')
        return candidates

    candidates, costs = measured(db, writer)
    changed = 0 if recorded_far else affected
    assert len(candidates) == changed
    assert db.execute(
        "SELECT COUNT(*) FROM selected_reply WHERE parent='message/root'"
    ).fetchone()[0] == affected
    for number in (0, affected // 2, affected - 1):
        child = children[number]
        assert exact_parent(db, child) == 'message/root'
        selected = db.execute(
            'SELECT parent FROM selected_reply WHERE child=?', (child,),
        ).fetchone()
        assert selected == ('message/root',)
    db.close()
    return {'mode': mode, 'scale': size, 'children': affected,
            'enumerated': len(candidates), 'changed': changed,
            'gap_node_reads': tree.node_reads,
            'gap_node_writes': tree.node_writes, **costs}


if __name__ == '__main__':
    assert sys.argv[1:] in (['--small-growth'], ['--growth'])
    scales = (10, 100) if sys.argv[1] == '--small-growth' else (1000, 10000)
    for mode in ('far-same-answer', 'all-changed', 'fixed-unrelated'):
        for size in scales:
            print(case(size, mode), flush=True)
