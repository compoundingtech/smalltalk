"""Exact mixed recorded/legacy reply order, with explicit affected-key cost.

Run with Python 3. This is an invented isolated SQLite model, not Store code.
"""

from __future__ import annotations

import sqlite3
import sys

ACTIVE_METRICS: dict[str, int] | None = None


SCHEMA = """
CREATE TABLE batches(id TEXT PRIMARY KEY, origin TEXT NOT NULL,
                     replica_sequence INTEGER NOT NULL);
CREATE TABLE claims(id TEXT PRIMARY KEY, store_index INTEGER NOT NULL UNIQUE,
                    batch_id TEXT NOT NULL, subject TEXT NOT NULL,
                    kind TEXT NOT NULL, parent TEXT,
                    accepted_at TEXT NOT NULL);
CREATE INDEX claims_batch_index ON claims(batch_id, store_index);
CREATE INDEX claims_subject_prefix ON claims(
  subject, length(accepted_at) DESC, accepted_at DESC, batch_id DESC, store_index DESC
);
CREATE INDEX claims_batch_subject ON claims(batch_id, kind, subject);
CREATE TABLE replica_records(record_ref TEXT PRIMARY KEY, claim_id TEXT NOT NULL,
                             position INTEGER NOT NULL, state TEXT NOT NULL);
CREATE INDEX records_claim_position ON replica_records(claim_id, position);
CREATE TABLE batch_rank_nodes(batch_id TEXT NOT NULL, bit INTEGER NOT NULL,
                              prefix INTEGER NOT NULL, count INTEGER NOT NULL,
                              PRIMARY KEY(batch_id, bit, prefix));
CREATE TABLE reply_assignments(
  claim_id TEXT PRIMARY KEY, store_index INTEGER NOT NULL, batch_id TEXT NOT NULL,
  accepted_at TEXT NOT NULL, origin TEXT NOT NULL, replica_sequence INTEGER NOT NULL,
  recorded_position INTEGER, parent TEXT, child TEXT NOT NULL,
  accepted_len INTEGER NOT NULL
);
CREATE INDEX reply_legacy_head ON reply_assignments(
  child,accepted_len DESC,accepted_at DESC,origin DESC,replica_sequence DESC,
  batch_id DESC,store_index DESC,claim_id DESC
) WHERE recorded_position IS NULL;
CREATE INDEX reply_recorded_head ON reply_assignments(
  child,accepted_len DESC,accepted_at DESC,origin DESC,replica_sequence DESC,
  batch_id DESC,recorded_position DESC,claim_id DESC
) WHERE recorded_position IS NOT NULL;
CREATE TABLE selected_reply(child TEXT PRIMARY KEY, parent TEXT NOT NULL);
"""


def rank_bump(db: sqlite3.Connection, batch: str, index: int, amount: int) -> None:
    """One node for each fixed-width binary prefix; 63 indexed point writes."""
    assert 0 < index < (1 << 63)
    if ACTIVE_METRICS is not None:
        ACTIVE_METRICS['rank_nodes_touched'] += 63
    for bit in range(63):
        prefix = index >> bit
        db.execute(
            "INSERT INTO batch_rank_nodes VALUES(?,?,?,?) "
            "ON CONFLICT(batch_id,bit,prefix) DO UPDATE SET count=count+excluded.count",
            (batch, bit, prefix, amount),
        )
        db.execute(
            "DELETE FROM batch_rank_nodes WHERE batch_id=? AND bit=? AND prefix=? AND count=0",
            (batch, bit, prefix),
        )


def prefix_rank(db: sqlite3.Connection, batch: str, index: int) -> int:
    """Count current batch claims before index in at most 63 indexed seeks."""
    rank = 0
    for bit in range(62, -1, -1):
        if index & (1 << bit):
            if ACTIVE_METRICS is not None:
                ACTIVE_METRICS['rank_point_seeks'] += 1
            sibling = (index >> bit) - 1
            row = db.execute(
                "SELECT count FROM batch_rank_nodes "
                "WHERE batch_id=? AND bit=? AND prefix=?", (batch, bit, sibling),
            ).fetchone()
            rank += row[0] if row else 0
    return rank


def slow_rank(db: sqlite3.Connection, batch: str, index: int) -> int:
    return db.execute(
        "SELECT COUNT(*) FROM claims WHERE batch_id=? AND store_index<?",
        (batch, index),
    ).fetchone()[0]


def full_key(db: sqlite3.Connection, row: tuple, fast: bool) -> tuple:
    claim_id, store_index, batch, accepted, origin, sequence, recorded = row
    position = recorded if recorded is not None else (
        prefix_rank(db, batch, store_index) if fast else slow_rank(db, batch, store_index)
    )
    return len(accepted), accepted, origin, sequence, batch, position, claim_id


def candidates(db: sqlite3.Connection, child: str) -> list[tuple]:
    return db.execute(
        "SELECT claims.id, claims.store_index, claims.batch_id, claims.accepted_at, "
        "batches.origin, batches.replica_sequence, "
        "(SELECT MIN(position) FROM replica_records WHERE claim_id=claims.id), "
        "claims.parent "
        "FROM claims JOIN batches ON batches.id=claims.batch_id "
        "WHERE claims.subject=? AND claims.kind='message.sent'", (child,),
    ).fetchall()


def sync_assignment(db: sqlite3.Connection, claim_id: str) -> None:
    db.execute("DELETE FROM reply_assignments WHERE claim_id=?", (claim_id,))
    db.execute(
        "INSERT INTO reply_assignments "
        "SELECT claims.id,claims.store_index,claims.batch_id,claims.accepted_at, "
        "batches.origin,batches.replica_sequence, "
        "(SELECT MIN(position) FROM replica_records WHERE claim_id=claims.id), "
        "claims.parent,claims.subject,length(claims.accepted_at) "
        "FROM claims JOIN batches ON batches.id=claims.batch_id "
        "WHERE claims.id=? AND claims.kind='message.sent'", (claim_id,),
    )


def exact_parent(db: sqlite3.Connection, child: str) -> str | None:
    """Independent canonical COUNT fold, excluded from writer counters."""
    rows = candidates(db, child)
    return max(rows, key=lambda row: full_key(db, row[:7], False))[7] if rows else None


def old_shortcut_parent(db: sqlite3.Connection, child: str) -> str | None:
    """The earlier unsafe store_index-vs-recorded-position ordering."""
    rows = candidates(db, child)
    if not rows:
        return None
    return max(rows, key=lambda row: (
        len(row[3]), row[3], row[4], row[5], row[2],
        row[6] if row[6] is not None else row[1], row[0],
    ))[7]


def lane_parent(db: sqlite3.Connection, child: str) -> str | None:
    """Highest canonical prefix, then one head from each position lane."""
    heads = []
    columns = ("claim_id,store_index,batch_id,accepted_at,origin,"
               "replica_sequence,recorded_position,parent")
    order = ("accepted_len DESC,accepted_at DESC,origin DESC,"
             "replica_sequence DESC,batch_id DESC,")
    for predicate, index, tail in [
        ("IS NULL", "reply_legacy_head", "store_index DESC,claim_id DESC"),
        ("IS NOT NULL", "reply_recorded_head", "recorded_position DESC,claim_id DESC"),
    ]:
        row = db.execute(
            f"SELECT {columns} FROM reply_assignments INDEXED BY {index} "
            f"WHERE child=? AND recorded_position {predicate} ORDER BY {order}{tail} LIMIT 1",
            (child,),
        ).fetchone()
        if row is not None:
            heads.append(row)
    return max(heads, key=lambda row: full_key(db, row[:7], True))[7] if heads else None


def refresh(db: sqlite3.Connection, child: str) -> None:
    db.execute("DELETE FROM selected_reply WHERE child=?", (child,))
    parent = lane_parent(db, child)
    if parent is not None:
        db.execute("INSERT INTO selected_reply VALUES(?,?)", (child, parent))


def all_children(db: sqlite3.Connection, batch: str) -> list[str]:
    """Complete but presently unbounded affected-key enumeration."""
    return [row[0] for row in db.execute(
        "SELECT DISTINCT subject FROM claims INDEXED BY claims_batch_subject "
        "WHERE batch_id=? AND kind='message.sent' ORDER BY subject", (batch,),
    )]


def check(db: sqlite3.Connection, children: list[str]) -> None:
    for child in children:
        assert lane_parent(db, child) == exact_parent(db, child), child
        stored = db.execute(
            "SELECT parent FROM selected_reply WHERE child=?", (child,),
        ).fetchone()
        assert (stored[0] if stored else None) == exact_parent(db, child), child
    for batch, index in db.execute("SELECT batch_id,store_index FROM claims"):
        assert prefix_rank(db, batch, index) == slow_rank(db, batch, index)


def add(db: sqlite3.Connection, claim_id: str, index: int, batch: str,
        child: str, parent: str | None, recorded: int | None = None) -> None:
    db.execute("INSERT OR IGNORE INTO batches VALUES(?,'host/example',1)", (batch,))
    db.execute("INSERT INTO claims VALUES(?,?,?,?,'message.sent',?,'100')",
               (claim_id, index, batch, child, parent))
    rank_bump(db, batch, index, 1)
    if recorded is not None:
        db.execute("INSERT INTO replica_records VALUES(?,?,?,'valid')",
                   ('record/' + claim_id, claim_id, recorded))
    sync_assignment(db, claim_id)
    refresh(db, child)


def mutate(db: sqlite3.Connection, batches: set[str], subjects: set[str], change,
           *, audit: bool = True) -> tuple[int, int]:
    """Enumerate both sides of every changed batch; correct, but not bounded."""
    old = sorted(subjects | {child for batch in batches for child in all_children(db, batch)})
    previous = {name: lane_parent(db, name) for name in old}
    change()
    affected = sorted(set(old) | subjects |
                      {child for batch in batches for child in all_children(db, batch)})
    for name in affected:
        refresh(db, name)
    if audit:
        check(db, affected)
    changed = sum(previous.get(name) != lane_parent(db, name) for name in affected)
    return len(affected), changed


def delete(db: sqlite3.Connection, claim_id: str, *, audit: bool = True) -> tuple[int, int]:
    batch, index, child, kind = db.execute(
        "SELECT batch_id,store_index,subject,kind FROM claims WHERE id=?", (claim_id,),
    ).fetchone()

    def change() -> None:
        db.execute("DELETE FROM replica_records WHERE claim_id=?", (claim_id,))
        db.execute("DELETE FROM reply_assignments WHERE claim_id=?", (claim_id,))
        db.execute("DELETE FROM claims WHERE id=?", (claim_id,))
        rank_bump(db, batch, index, -1)

    return mutate(db, {batch}, {child} if kind == 'message.sent' else set(),
                  change, audit=audit)


def measured(db: sqlite3.Connection, work) -> tuple[object, dict[str, int]]:
    global ACTIVE_METRICS
    assert ACTIVE_METRICS is None
    metrics = {'vm_steps': 0, 'statements': 0, 'sql_trace_bytes': 0,
               'rank_nodes_touched': 0, 'rank_point_seeks': 0}
    ACTIVE_METRICS = metrics
    before_changes = db.total_changes
    page_size = db.execute('PRAGMA page_size').fetchone()[0]
    before_pages = db.execute('PRAGMA page_count').fetchone()[0]

    def tick() -> int:
        metrics['vm_steps'] += 1
        return 0

    def traced(statement: str) -> None:
        metrics['statements'] += 1
        metrics['sql_trace_bytes'] += len(statement.encode())

    db.set_progress_handler(tick, 1)
    db.set_trace_callback(traced)
    try:
        answer = work()
    finally:
        db.set_progress_handler(None, 0)
        db.set_trace_callback(None)
        ACTIVE_METRICS = None
    metrics['row_changes'] = db.total_changes - before_changes
    metrics['allocated_page_bytes_delta'] = (
        db.execute('PRAGMA page_count').fetchone()[0] - before_pages
    ) * page_size
    return answer, metrics


def growth(scale: int) -> dict:
    """One unrelated deletion flips every mixed child in this invented batch."""
    db = sqlite3.connect(':memory:')
    db.executescript(SCHEMA)
    batch = 'batch/growth/1'
    db.execute("INSERT INTO batches VALUES(?,'host/example',1)", (batch,))
    db.execute("INSERT INTO claims VALUES('predecessor',1,?,'resource/example',"
               "'resource.observed',NULL,'100')", (batch,))
    rank_bump(db, batch, 1, 1)
    for number in range(scale):
        child = f'message/child-{number:05}'
        add(db, f'z-{number:05}', 2 * number + 2, batch, child, 'message/root',
            recorded=2 * number + 1)
        add(db, f'a-{number:05}', 2 * number + 3, batch, child, 'message/other')
    assert db.execute("SELECT COUNT(*) FROM selected_reply "
                      "WHERE parent='message/other'").fetchone()[0] == scale
    (enumerated, changed), metrics = measured(
        db, lambda: delete(db, 'predecessor', audit=False),
    )
    assert (enumerated, changed) == (scale, scale), (scale, enumerated, changed)
    assert db.execute("SELECT COUNT(*) FROM selected_reply "
                      "WHERE parent='message/root'").fetchone()[0] == scale
    # Independent canonical spot checks are outside the measured writer scope;
    # SQL's selected-parent count above checks the whole returned population.
    for number in (0, scale // 2, scale - 1):
        child = f'message/child-{number:05}'
        assert lane_parent(db, child) == exact_parent(db, child)
        index = 2 * number + 3
        assert prefix_rank(db, batch, index) == slow_rank(db, batch, index)
    db.close()
    return {'case': 'all-changed', 'scale': scale, 'enumerated': enumerated,
            'changed': changed, **metrics}


def fixed_answer_unrelated(scale: int) -> dict:
    """Two affected children while another batch's retained history grows."""
    db = sqlite3.connect(':memory:')
    db.executescript(SCHEMA)
    target = 'batch/fixed/1'
    db.execute("INSERT INTO batches VALUES(?,'host/example',1)", (target,))
    db.execute("INSERT INTO claims VALUES('predecessor',1,?,'resource/example',"
               "'resource.observed',NULL,'100')", (target,))
    rank_bump(db, target, 1, 1)
    for number in range(2):
        child = f'message/target-{number}'
        add(db, f'z-target-{number}', 2 * number + 2, target, child,
            'message/root', recorded=2 * number + 1)
        add(db, f'a-target-{number}', 2 * number + 3, target, child,
            'message/other')
    unrelated = 'batch/unrelated/1'
    db.execute("INSERT INTO batches VALUES(?,'host/example',2)", (unrelated,))
    for number in range(scale):
        index = 100 + number
        db.execute("INSERT INTO claims VALUES(?,?,?,'resource/unrelated',"
                   "'resource.observed',NULL,'100')",
                   (f'unrelated-{number}', index, unrelated))
        rank_bump(db, unrelated, index, 1)
    (enumerated, changed), metrics = measured(
        db, lambda: delete(db, 'predecessor', audit=False),
    )
    assert (enumerated, changed) == (2, 2)
    check(db, ['message/target-0', 'message/target-1'])
    db.close()
    return {'case': 'fixed-answer-unrelated-history', 'scale': scale,
            'enumerated': enumerated, 'changed': changed, **metrics}


def unchanged_batch(scale: int) -> dict:
    """A changed batch with many children whose selected parents stay put."""
    db = sqlite3.connect(':memory:')
    db.executescript(SCHEMA)
    batch = 'batch/unchanged/1'
    db.execute("INSERT INTO batches VALUES(?,'host/example',1)", (batch,))
    db.execute("INSERT INTO claims VALUES('predecessor',1,?,'resource/example',"
               "'resource.observed',NULL,'100')", (batch,))
    rank_bump(db, batch, 1, 1)
    for number in range(scale):
        add(db, f'z-{number:05}', number + 2, batch,
            f'message/unchanged-{number:05}', 'message/root', recorded=number + 1)
    (enumerated, changed), metrics = measured(
        db, lambda: delete(db, 'predecessor', audit=False),
    )
    assert (enumerated, changed) == (scale, 0)
    assert db.execute("SELECT COUNT(*) FROM selected_reply "
                      "WHERE parent='message/root'").fetchone()[0] == scale
    for number in (0, scale // 2, scale - 1):
        child = f'message/unchanged-{number:05}'
        assert lane_parent(db, child) == exact_parent(db, child)
    db.close()
    return {'case': 'same-answer-changed-batch', 'scale': scale,
            'enumerated': enumerated, 'changed': changed, **metrics}


def main() -> None:
    db = sqlite3.connect(':memory:')
    db.executescript(SCHEMA)
    batch = 'batch/example/1'
    add(db, 'a-first', 1, batch, 'message/child', 'message/other')
    add(db, 'z-recorded', 2, batch, 'message/child', 'message/root', recorded=2)
    # The old store_index shortcut selects the legacy claim (1 or 3 versus 2)
    # even though canonical COUNT chooses the recorded claim at position 2.
    assert exact_parent(db, 'message/child') == 'message/root'
    assert lane_parent(db, 'message/child') == 'message/root'
    add(db, 'a-late', 3, batch, 'message/child', 'message/other')
    assert exact_parent(db, 'message/child') == 'message/root'
    assert lane_parent(db, 'message/child') == 'message/root'
    assert old_shortcut_parent(db, 'message/child') == 'message/other'
    assert prefix_rank(db, batch, 3) == slow_rank(db, batch, 3) == 2
    # Record state does not remove a retained message.sent claim from the fold.
    db.execute("UPDATE replica_records SET state='repaired' WHERE claim_id='z-recorded'")
    refresh(db, 'message/child')
    check(db, ['message/child'])
    # Record identity and position updates select both old and new claim subjects.
    second = 'message/second'
    add(db, 'a-second', 4, batch, second, 'message/other')
    other_batch = 'batch/example/2'
    third = 'message/third'
    add(db, 'a-third', 6, other_batch, third, 'message/elsewhere')
    def move_record(target: str, position: int) -> None:
        db.execute("UPDATE replica_records SET claim_id=?,position=? "
                   "WHERE record_ref='record/z-recorded'", (target, position))
        sync_assignment(db, 'z-recorded')
        sync_assignment(db, 'a-second')
        sync_assignment(db, 'a-third')

    assert mutate(db, {batch}, {'message/child', second},
                  lambda: move_record('a-second', 0))[0] == 2
    assert mutate(db, {batch}, {'message/child', second},
                  lambda: move_record('z-recorded', 2))[0] == 2
    assert mutate(db, {batch, other_batch}, {'message/child', third},
                  lambda: move_record('a-third', 0))[0] == 3
    assert mutate(db, {batch, other_batch}, {'message/child', third},
                  lambda: move_record('z-recorded', 2))[0] == 3
    check(db, ['message/child', second, third])

    def move_index() -> None:
        rank_bump(db, batch, 4, -1)
        db.execute("UPDATE claims SET store_index=5 WHERE id='a-second'")
        rank_bump(db, batch, 5, 1)
        sync_assignment(db, 'a-second')

    assert mutate(db, {batch}, {second}, move_index)[0] == 2
    def insert_predecessor() -> None:
        db.execute("INSERT INTO claims VALUES('new-predecessor',4,?,'resource/example',"
                   "'resource.observed',NULL,'100')", (batch,))
        rank_bump(db, batch, 4, 1)

    assert mutate(db, {batch}, set(), insert_predecessor)[0] == 2
    affected, changed = delete(db, 'a-first')
    assert affected == 2 and changed <= affected
    print('mixed lane heads, repaired record, exact rank and affected-child audit passed',
          {'enumerated': affected, 'changed': changed})


if __name__ == '__main__':
    main()
    if '--growth' in sys.argv or '--small-growth' in sys.argv:
        scales = (1000, 10000) if '--growth' in sys.argv else (10, 100)
        for case in (growth, fixed_answer_unrelated, unchanged_batch):
            print([case(scale) for scale in scales])
