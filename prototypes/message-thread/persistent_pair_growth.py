"""Queued two-scale SQL writer-cost driver for persisted rank gaps.

DO NOT treat source or parser checks as an executed cost receipt. Running this
large fixture needs a fresh serialized model grant and whole-unit cap.
"""

from __future__ import annotations

import sqlite3
import sys

from mixed_rank_oracle import (SCHEMA as MIXED_SCHEMA, add, exact_parent,
                               measured, rank_bump, refresh, sync_assignment)
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
    recorded_far = mode in ('far-same-answer', 'direct-replacement')
    same_parent = mode == 'same-parent-near-zero'
    children = []
    for number in range(affected):
        child = f'message/cost-{number:05}'
        children.append(child)
        recorded_position = 100_000 if recorded_far else 2 * number + 1
        add(db, f'z-{number:05}', 2 * number + 2, target,
            child, 'message/root', recorded=recorded_position)
        add(db, f'a-{number:05}', 2 * number + 3, target,
            child, 'message/root' if same_parent else 'message/other')
        refresh_child_leaf(db, child)
    if same_parent:
        # Every comparison is one rank shift from a tie, but parent identity
        # makes all of them irrelevant to selected_reply membership.
        assert db.execute('SELECT COUNT(*) FROM reply_gap_leaves').fetchone()[0] == 0
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
        if mode == 'direct-replacement':
            db.execute("UPDATE replica_records SET position=0 "
                       "WHERE claim_id='z-00000'")
            sync_assignment(db, 'z-00000')
            refresh_child_leaf(db, children[0])
            refresh(db, children[0])
            db.execute('RELEASE selected_reply_writer')
            return [children[0]]
        candidates = tree.shift_after(1, -1)
        db.execute("DELETE FROM claims WHERE id='predecessor'")
        rank_bump(db, target, 1, -1)
        for child in candidates:
            refresh(db, child)
        db.execute('RELEASE selected_reply_writer')
        return candidates

    candidates, costs = measured(db, writer)
    changed = (1 if mode == 'direct-replacement' else
               0 if recorded_far or same_parent else affected)
    assert len(candidates) == changed
    expected_root = affected - 1 if mode == 'direct-replacement' else affected
    assert db.execute(
        "SELECT COUNT(*) FROM selected_reply WHERE parent='message/root'"
    ).fetchone()[0] == expected_root
    for number in (0, affected // 2, affected - 1):
        child = children[number]
        expected_parent = ('message/other' if
                           mode == 'direct-replacement' and number == 0
                           else 'message/root')
        assert exact_parent(db, child) == expected_parent
        selected = db.execute(
            'SELECT parent FROM selected_reply WHERE child=?', (child,),
        ).fetchone()
        assert selected == (expected_parent,)
    db.close()
    return {'mode': mode, 'scale': size, 'children': affected,
            'enumerated': len(candidates), 'changed': changed,
            # `measured` traces the connection, including temporary OLD and
            # NEW tree instances, direct leaf SQL, rank nodes and selected rows.
            # Per-object counters would omit the OLD tree in replace_head.
            **costs}


if __name__ == '__main__':
    assert sys.argv[1:] in (['--small-growth'], ['--growth'])
    scales = (10, 100) if sys.argv[1] == '--small-growth' else (1000, 10000)
    for mode in ('far-same-answer', 'same-parent-near-zero',
                 'all-changed', 'fixed-unrelated', 'direct-replacement'):
        for size in scales:
            print(case(size, mode), flush=True)
