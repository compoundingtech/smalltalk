"""Small semantic coupling of the rank-gap tree to the SQLite reply model.

No growth or performance claim follows from this invented in-memory fixture.
The range tree is not persisted and its work is outside SQLite VM counters.
"""

from __future__ import annotations

import sqlite3

from mixed_rank_oracle import (SCHEMA, add, exact_parent, lane_parent,
                               prefix_rank, rank_bump, refresh, sync_assignment)
from rank_gap_tree_oracle import GapTree


def exercise(size: int, far_recorded: bool) -> dict[str, int]:
    db = sqlite3.connect(':memory:')
    db.executescript(SCHEMA)
    batch = 'batch/coupled/1'
    db.execute("INSERT INTO batches VALUES(?,'host/example',1)", (batch,))
    db.execute("INSERT INTO claims VALUES('predecessor',1,?,'resource/example',"
               "'resource.observed',NULL,'100')", (batch,))
    rank_bump(db, batch, 1, 1)
    tree = GapTree()
    children: dict[int, str] = {}
    before: dict[str, str | None] = {}
    for number in range(size):
        child = f'message/coupled-{number:05}'
        recorded_position = 100_000 if far_recorded else 2 * number + 1
        add(db, f'z-{number:05}', 2 * number + 2, batch, child,
            'message/root', recorded=recorded_position)
        legacy_index = 2 * number + 3
        add(db, f'a-{number:05}', legacy_index, batch, child,
            'message/other')
        tree.set(legacy_index,
                 prefix_rank(db, batch, legacy_index) - recorded_position)
        children[legacy_index] = child
        before[child] = exact_parent(db, child)
    assert all(lane_parent(db, child) == before[child]
               for child in children.values())

    # One publication cut: shift the range summary and persisted rank index,
    # then refresh only children whose lane comparison might cross.
    candidates, visits = tree.shift_after(1, -1)
    db.execute("DELETE FROM claims WHERE id='predecessor'")
    rank_bump(db, batch, 1, -1)
    for index in candidates:
        refresh(db, children[index])
    after = {child: exact_parent(db, child) for child in children.values()}
    assert all(lane_parent(db, child) == parent for child, parent in after.items())
    assert all(db.execute("SELECT parent FROM selected_reply WHERE child=?", (child,))
               .fetchone()[0] == parent for child, parent in after.items())
    changed = sum(before[child] != parent for child, parent in after.items())
    assert (len(candidates), changed) == ((0, 0) if far_recorded
                                          else (size, size))

    # An OLD/NEW replica-record change is a direct child key update. Its new
    # lane gap is computed from the current exact batch rank at the same cut.
    direct_child = children[3]
    old_recorded = db.execute(
        "SELECT MIN(position) FROM replica_records WHERE claim_id='z-00000'"
    ).fetchone()[0]
    db.execute("UPDATE replica_records SET position=? WHERE claim_id='z-00000'",
               (100_000 if old_recorded < 100_000 else 0,))
    sync_assignment(db, 'z-00000')
    current_recorded = db.execute(
        "SELECT MIN(position) FROM replica_records WHERE claim_id='z-00000'"
    ).fetchone()[0]
    tree.set(3, prefix_rank(db, batch, 3) - current_recorded)
    refresh(db, direct_child)
    assert lane_parent(db, direct_child) == exact_parent(db, direct_child)
    assert db.execute("SELECT parent FROM selected_reply WHERE child=?",
                      (direct_child,)).fetchone()[0] == exact_parent(db, direct_child)

    db.close()
    return {'children': size, 'far_recorded': int(far_recorded),
            'enumerated': len(candidates), 'changed': changed,
            'range_node_visits': visits}


if __name__ == '__main__':
    for size in (10, 100):
        for far in (True, False):
            print(exercise(size, far))
