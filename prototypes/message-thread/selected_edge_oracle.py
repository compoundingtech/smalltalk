"""Executable SQLite spike for a transactional selected message child->parent index.

This uses the graph's canonical tuple order and the message view's actual-field
precedence. It is isolated from Store; production migration and cost remain open.
"""

from __future__ import annotations

import json
import sqlite3
import tempfile
from pathlib import Path


ORDER = ("accepted_len DESC, accepted_at DESC, origin DESC, "
         "replica_sequence DESC, batch_id DESC, position DESC, claim_id DESC")


def refresh(subject: str) -> str:
    normalized = lambda value: ("CASE WHEN substr(" + value + ",1,8)='message/' "
                                "THEN " + value + " ELSE 'message/' || " + value + " END")
    return f"""
        DELETE FROM selected_reply WHERE child={subject};
        INSERT INTO selected_reply(child,parent)
        SELECT child,{normalized('parent')} FROM (
          SELECT child,parent FROM reply_assignments
          WHERE child={subject} ORDER BY {ORDER} LIMIT 1
        ) WHERE typeof(parent)='text';
        INSERT OR IGNORE INTO selected_reply(child,parent)
        SELECT desired.subject,{normalized("json_extract(edge.value,'$.arguments[0]')")}
        FROM desired JOIN json_each(desired.body,'$.children') AS edge
        WHERE desired.subject={subject} AND desired.kind='message'
          AND json_extract(edge.value,'$.name')='in-reply-to'
          AND CAST(edge.key AS INTEGER)=(
            SELECT MIN(CAST(first_edge.key AS INTEGER))
            FROM json_each(desired.body,'$.children') AS first_edge
            WHERE json_extract(first_edge.value,'$.name')='in-reply-to'
          )
          AND json_type(edge.value,'$.arguments[0]')='text'
          AND NOT EXISTS(SELECT 1 FROM reply_assignments
                         WHERE child=desired.subject);
    """


SCHEMA = """
PRAGMA foreign_keys=ON;
CREATE TABLE batches(id TEXT PRIMARY KEY,origin TEXT NOT NULL,
                     replica_sequence INTEGER NOT NULL);
CREATE TABLE claims(id TEXT PRIMARY KEY,store_index INTEGER NOT NULL UNIQUE,
                    batch_id TEXT NOT NULL REFERENCES batches(id),
                    subject TEXT NOT NULL,kind TEXT NOT NULL,body TEXT NOT NULL,
                    accepted_at_unix_ms TEXT NOT NULL);
CREATE INDEX claims_subject_kind ON claims(subject,kind,store_index);
CREATE INDEX claims_batch_index ON claims(batch_id,store_index);
CREATE TABLE replica_records(record_ref TEXT PRIMARY KEY,
                             claim_id TEXT NOT NULL REFERENCES claims(id),
                             position INTEGER NOT NULL,
                             state TEXT NOT NULL DEFAULT 'active');
CREATE INDEX replica_records_claim_position ON replica_records(claim_id,position);
CREATE TABLE desired(subject TEXT PRIMARY KEY,kind TEXT NOT NULL,body TEXT NOT NULL);
CREATE TABLE reply_assignments(
  claim_id TEXT PRIMARY KEY,child TEXT NOT NULL,parent TEXT,
  accepted_len INTEGER NOT NULL,accepted_at TEXT NOT NULL,origin TEXT NOT NULL,
  replica_sequence INTEGER NOT NULL,batch_id TEXT NOT NULL,position INTEGER NOT NULL
);
CREATE INDEX reply_assignment_predecessor ON reply_assignments(
  child,accepted_len DESC,accepted_at DESC,origin DESC,
  replica_sequence DESC,batch_id DESC,position DESC,claim_id DESC
);
CREATE TABLE selected_reply(child TEXT PRIMARY KEY,parent TEXT NOT NULL);
CREATE INDEX selected_reply_parent_child ON selected_reply(parent,child);
"""


def triggers(db: sqlite3.Connection) -> None:
    def insert_assignment(claim_id: str) -> str:
        return f"""
      INSERT INTO reply_assignments
      (claim_id,child,parent,accepted_len,accepted_at,origin,replica_sequence,batch_id,position)
      SELECT claims.id,claims.subject,json_extract(claims.body,'$.fields.in_reply_to'),
             length(claims.accepted_at_unix_ms),claims.accepted_at_unix_ms,
             batches.origin,batches.replica_sequence,claims.batch_id,
             COALESCE((SELECT MIN(position) FROM replica_records WHERE claim_id=claims.id),
               claims.store_index)
      FROM claims JOIN batches ON batches.id=claims.batch_id
      WHERE claims.id={claim_id} AND claims.kind='message.sent'
        AND json_type(claims.body,'$.fields.in_reply_to') IS NOT NULL;
    """
    db.executescript(f"""
      CREATE TRIGGER reply_claim_insert AFTER INSERT ON claims
      WHEN NEW.kind='message.sent' BEGIN
        {insert_assignment('NEW.id')}
        {refresh('NEW.subject')}
      END;
      CREATE TRIGGER reply_claim_delete AFTER DELETE ON claims
      WHEN OLD.kind='message.sent' BEGIN
        DELETE FROM reply_assignments WHERE claim_id=OLD.id;
        {refresh('OLD.subject')}
      END;
      CREATE TRIGGER reply_desired_insert AFTER INSERT ON desired BEGIN
        {refresh('NEW.subject')}
      END;
      CREATE TRIGGER reply_desired_update AFTER UPDATE ON desired BEGIN
        {refresh('OLD.subject')}
        {refresh('NEW.subject')}
      END;
      CREATE TRIGGER reply_desired_delete AFTER DELETE ON desired BEGIN
        {refresh('OLD.subject')}
      END;
      CREATE TRIGGER reply_record_insert AFTER INSERT ON replica_records BEGIN
        DELETE FROM reply_assignments WHERE claim_id=NEW.claim_id;
        {insert_assignment('NEW.claim_id')}
        {refresh('(SELECT subject FROM claims WHERE id=NEW.claim_id)')}
      END;
      CREATE TRIGGER reply_record_update AFTER UPDATE ON replica_records BEGIN
        DELETE FROM reply_assignments WHERE claim_id=NEW.claim_id;
        {insert_assignment('NEW.claim_id')}
        {refresh('(SELECT subject FROM claims WHERE id=NEW.claim_id)')}
      END;
      CREATE TRIGGER reply_record_delete AFTER DELETE ON replica_records BEGIN
        DELETE FROM reply_assignments WHERE claim_id=OLD.claim_id;
        {insert_assignment('OLD.claim_id')}
        {refresh('(SELECT subject FROM claims WHERE id=OLD.claim_id)')}
      END;
    """)


def vm_steps(db: sqlite3.Connection, run):
    steps = 0

    def tick():
        nonlocal steps
        steps += 1
        return 0

    db.set_progress_handler(tick, 1)
    try:
        result = run()
    finally:
        db.set_progress_handler(None, 0)
    return result, steps


def desired(parent: str | None, first_invalid: bool = False) -> str:
    edges = ([{"name": "in-reply-to", "arguments": [42]}] if first_invalid else [])
    if parent is not None:
        edges.append({"name": "in-reply-to", "arguments": [parent]})
    return json.dumps({"children": edges})


def selected(db: sqlite3.Connection, child: str) -> str | None:
    row = db.execute("SELECT parent FROM selected_reply WHERE child=?", (child,)).fetchone()
    return None if row is None else row[0]


def exact_parent(db: sqlite3.Connection, child: str) -> str | None:
    """Small-fixture independent fold using canonical.rs's COUNT fallback."""
    candidates = []
    rows = db.execute(
        "SELECT claims.id,claims.store_index,claims.batch_id,claims.body,"
        "claims.accepted_at_unix_ms,batches.origin,batches.replica_sequence "
        "FROM claims JOIN batches ON batches.id=claims.batch_id "
        "WHERE claims.subject=? AND claims.kind='message.sent'", (child,))
    for claim_id, store_index, batch, body, accepted, origin, sequence in rows:
        value = json.loads(body).get("fields", {})
        if "in_reply_to" not in value:
            continue
        records = db.execute(
            "SELECT position FROM replica_records WHERE claim_id=?", (claim_id,)
        ).fetchall()
        position = min((position for (position,) in records), default=None)
        if position is None:
            position = db.execute(
                "SELECT COUNT(*) FROM claims WHERE batch_id=? AND store_index<?",
                (batch, store_index),
            ).fetchone()[0]
        key = (len(accepted), accepted, origin, sequence, batch, position, claim_id)
        candidates.append((key, value["in_reply_to"]))
    if candidates:
        parent = max(candidates)[1]
    else:
        row = db.execute("SELECT body FROM desired WHERE subject=? AND kind='message'",
                         (child,)).fetchone()
        parent = None
        if row:
            for edge in json.loads(row[0]).get("children", []):
                if edge.get("name") == "in-reply-to":
                    first = edge.get("arguments", [None])
                    parent = first[0] if first else None
                    break
    if not isinstance(parent, str):
        return None
    return parent if parent.startswith("message/") else "message/" + parent


def assert_complete_batch_modes(db: sqlite3.Connection) -> None:
    for batch, total, recorded in db.execute(
        "SELECT batches.id,COUNT(*),SUM(EXISTS(SELECT 1 FROM replica_records "
        "WHERE replica_records.claim_id=claims.id)) "
        "FROM batches JOIN claims ON claims.batch_id=batches.id GROUP BY batches.id"
    ):
        assert recorded in (0, total), (batch, total, recorded)


def rebuild(db: sqlite3.Connection) -> None:
    """Model a versioned local-cache rebuild from the current message fold."""
    subjects = [row[0] for row in db.execute(
        "SELECT subject FROM desired UNION SELECT subject FROM claims "
        "WHERE kind='message.sent'")]
    statements = """BEGIN IMMEDIATE;
      DELETE FROM selected_reply;
      DELETE FROM reply_assignments;
      INSERT INTO reply_assignments
      (claim_id,child,parent,accepted_len,accepted_at,origin,replica_sequence,batch_id,position)
      SELECT claims.id,claims.subject,json_extract(claims.body,'$.fields.in_reply_to'),
             length(claims.accepted_at_unix_ms),claims.accepted_at_unix_ms,
             batches.origin,batches.replica_sequence,claims.batch_id,
             COALESCE((SELECT MIN(position) FROM replica_records WHERE claim_id=claims.id),
               claims.store_index)
      FROM claims JOIN batches ON batches.id=claims.batch_id
      WHERE claims.kind='message.sent'
        AND json_type(claims.body,'$.fields.in_reply_to') IS NOT NULL;
    """
    for subject in subjects:
        quoted = db.execute("SELECT quote(?)", (subject,)).fetchone()[0]
        statements += refresh(quoted)
    db.executescript(statements + "COMMIT;")


def claim(db: sqlite3.Connection, number: int, child: str, parent: str | None,
          *, at: int = 100, batch: str = "a", present: bool = True) -> str:
    claim_id = f"claim-{number:06}"
    fields = {"in_reply_to": parent} if present else {}
    db.execute("INSERT OR IGNORE INTO batches VALUES(?,'host/a',1)", (batch,))
    db.execute(
        "INSERT INTO claims VALUES(?,?,?,?,?,?,?)",
        (claim_id, number, batch, child, "message.sent",
         json.dumps({"fields": fields}), str(at)),
    )
    return claim_id


def main() -> None:
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory, "selected.sqlite3")
        db = sqlite3.connect(path)
        db.executescript(SCHEMA)
        triggers(db)
        db.execute("INSERT INTO batches VALUES('a','host/a',1)")
        db.execute("INSERT INTO batches VALUES('b','host/b',1)")
        child = "message/child"
        db.execute("INSERT INTO desired VALUES(?,?,?)",
                   (child, "message", desired("root")))
        assert selected(db, child) == "message/root"
        assert selected(db, child) == exact_parent(db, child)
        first = claim(db, 1, child, "other", at=100)
        assert selected(db, child) == "message/other"
        second = claim(db, 2, child, "root", at=100, batch="b")
        assert selected(db, child) == "message/root"
        # Replica position participates after batch ID. A second claim in the
        # same batch can reorder without a claim insert/delete.
        third = claim(db, 3, child, "later", at=100, batch="b")
        assert selected(db, child) == "message/later"
        assert selected(db, child) == exact_parent(db, child)
        assert_complete_batch_modes(db)
        db.execute("INSERT INTO replica_records(record_ref,claim_id,position) VALUES('r2',?,0)",
                   (second,))
        db.execute("INSERT INTO replica_records(record_ref,claim_id,position) VALUES('r3a',?,-1)",
                   (third,))
        assert selected(db, child) == "message/root"
        assert selected(db, child) == exact_parent(db, child)
        assert_complete_batch_modes(db)
        db.execute("INSERT INTO replica_records(record_ref,claim_id,position) VALUES('r3b',?,-2)",
                   (third,))
        assert selected(db, child) == "message/root"
        db.execute("UPDATE replica_records SET position=3 WHERE record_ref='r3a'")
        assert selected(db, child) == "message/root"  # second record still selects -2
        db.execute("UPDATE replica_records SET position=4 WHERE record_ref='r3b'")
        assert selected(db, child) == "message/later"
        assert selected(db, child) == exact_parent(db, child)
        db.execute("UPDATE replica_records SET state='repaired' WHERE record_ref='r3b'")
        assert selected(db, child) == "message/later"  # repair retains the claim in the fold
        assert selected(db, child) == exact_parent(db, child)
        db.execute("UPDATE replica_records SET state='active' WHERE record_ref='r3b'")
        assert selected(db, child) == "message/later"
        db.execute("DELETE FROM replica_records WHERE record_ref='r3b'")
        assert selected(db, child) == "message/later"
        db.execute("DELETE FROM replica_records WHERE record_ref='r3a'")
        db.execute("DELETE FROM replica_records WHERE record_ref='r2'")
        assert selected(db, child) == "message/later"
        assert selected(db, child) == exact_parent(db, child)
        assert_complete_batch_modes(db)
        db.execute("DELETE FROM claims WHERE id=?", (third,))
        assert selected(db, child) == "message/root"
        db.execute("DELETE FROM claims WHERE id=?", (second,))
        assert selected(db, child) == "message/other"
        db.execute("DELETE FROM claims WHERE id=?", (first,))
        assert selected(db, child) == "message/root"
        db.execute("UPDATE desired SET body=? WHERE subject=?",
                   (desired("wrong", first_invalid=True), child))
        assert selected(db, child) is None  # first matching child is invalid
        db.execute("UPDATE desired SET body=? WHERE subject=?", (desired("root"), child))
        null = claim(db, 4, child, None, at=110)
        assert selected(db, child) is None  # explicit null masks desired
        assert selected(db, child) == exact_parent(db, child)
        db.execute("DELETE FROM claims WHERE id=?", (null,))
        assert selected(db, child) == "message/root"
        missing = claim(db, 5, child, None, at=111, present=False)
        assert selected(db, child) == "message/root"  # absent field falls back
        assert selected(db, child) == exact_parent(db, child)
        db.execute("DELETE FROM claims WHERE id=?", (missing,))

        legacy = "message/legacy-delete"
        for number, parent in [(6, "root"), (7, "other"), (8, "root")]:
            claim(db, number, legacy, parent, at=120, batch="legacy")
        assert selected(db, legacy) == exact_parent(db, legacy) == "message/root"
        db.execute("DELETE FROM claims WHERE id='claim-000006'")
        assert selected(db, legacy) == exact_parent(db, legacy) == "message/root"
        db.commit()
        db.execute("BEGIN IMMEDIATE")
        db.execute("INSERT INTO replica_records(record_ref,claim_id,position) "
                   "VALUES('invalid-mixed','claim-000008',2)")
        assert selected(db, legacy) == "message/other"
        assert exact_parent(db, legacy) == "message/root"
        try:
            assert_complete_batch_modes(db)
        except AssertionError:
            pass
        else:
            raise AssertionError("mixed recorded/legacy batch must fail mode validation")
        db.rollback()
        assert_complete_batch_modes(db)
        db.execute("DELETE FROM claims WHERE subject=?", (legacy,))
        assert selected(db, legacy) is None

        db.commit()
        db.execute("BEGIN IMMEDIATE")
        db.execute("UPDATE desired SET body=? WHERE subject=?", (desired("rollback"), child))
        assert selected(db, child) == "message/rollback"
        db.rollback()
        assert selected(db, child) == "message/root"
        db.execute("DELETE FROM selected_reply")
        rebuild(db)
        assert selected(db, child) == "message/root"

        db.commit()
        db.close()
        db = sqlite3.connect(path)
        assert selected(db, child) == "message/root"  # reopen uses persisted index
        plan = db.execute("EXPLAIN QUERY PLAN SELECT child FROM selected_reply "
                          "INDEXED BY selected_reply_parent_child "
                          "WHERE parent=? ORDER BY child LIMIT 201", ("message/root",)).fetchall()
        assert any("selected_reply_parent_child" in row[3] for row in plan), plan

        def children():
            return [row[0] for row in db.execute(
                "SELECT child FROM selected_reply INDEXED BY selected_reply_parent_child "
                "WHERE parent=? ORDER BY child LIMIT 201", ("message/root",))]

        costs = []
        for scale, previous in [(1000, 0), (10000, 1000)]:
            for index in range(previous, scale):
                stale = f"message/stale-{index:05}"
                db.execute("INSERT INTO desired VALUES(?,?,?)",
                           (stale, "message", desired("root")))
                claim(db, 100 + index, stale, "other", at=1000 + index,
                      batch=f"stale-{index // 100}")
            answer, read_steps = vm_steps(db, children)
            assert answer == [child]
            probe = f"message/probe-{scale}"
            _, write_steps = vm_steps(db, lambda: db.execute(
                "INSERT INTO desired VALUES(?,?,?)",
                (probe, "message", desired("root"))))
            assert selected(db, probe) == "message/root"
            db.execute("DELETE FROM desired WHERE subject=?", (probe,))
            costs.append((scale, read_steps, write_steps))
        assert costs[1][1] < costs[0][1] * 4, costs
        assert costs[1][2] < costs[0][2] * 4, costs
        print("1k→10k stale subjects: scale/read-VM-steps/desired-write-VM-steps", costs)

        history = "message/history"
        history_costs = []
        for scale, previous in [(1000, 0), (10000, 1000)]:
            for index in range(previous, scale):
                claim(db, 20000 + index, history,
                      "root" if index % 2 == 0 else "other",
                      at=20000 + index, batch=f"history-{index // 100}")
            last = f"claim-{20000 + scale - 1:06}"
            _, delete_steps = vm_steps(db, lambda: db.execute(
                "DELETE FROM claims WHERE id=?", (last,)))
            assert selected(db, history) == "message/root"
            _, insert_steps = vm_steps(db, lambda: claim(
                db, 20000 + scale - 1, history, "other",
                at=20000 + scale - 1, batch=f"history-{(scale - 1) // 100}"))
            assert selected(db, history) == "message/other"
            history_costs.append((scale, delete_steps, insert_steps))
        assert history_costs[1][1] < history_costs[0][1] * 4, history_costs
        assert history_costs[1][2] < history_costs[0][2] * 4, history_costs
        print("1k→10k one-child assignments: scale/delete-VM-steps/insert-VM-steps", history_costs)

        legacy_history = "message/large-legacy-batch"
        large_batch_costs = []
        for scale, previous in [(1000, 0), (10000, 1000)]:
            for index in range(previous, scale):
                claim(db, 40000 + index, legacy_history,
                      "root" if index % 2 == 0 else "other",
                      at=30000, batch="huge")
            assert selected(db, legacy_history) == "message/other"
            _, insert_steps = vm_steps(db, lambda: claim(
                db, 40000 + scale, legacy_history, "root", at=30000, batch="huge"))
            assert selected(db, legacy_history) == "message/root"
            added = f"claim-{40000 + scale:06}"
            _, delete_steps = vm_steps(db, lambda: db.execute(
                "DELETE FROM claims WHERE id=?", (added,)))
            assert selected(db, legacy_history) == "message/other"
            large_batch_costs.append((scale, insert_steps, delete_steps))
        assert large_batch_costs[1][1] < large_batch_costs[0][1] * 4, large_batch_costs
        assert large_batch_costs[1][2] < large_batch_costs[0][2] * 4, large_batch_costs
        print("1k→10k one legacy batch: scale/insert-VM-steps/delete-VM-steps", large_batch_costs)
        db.commit()
        rebuild(db)
        assert children() == [child]
        assert selected(db, history) == "message/other"
        assert selected(db, legacy_history) == "message/other"
        db.close()
    print("selected precedence, null masking, reorder, deletion, desired, reopen and indexed seek passed")


if __name__ == "__main__":
    main()
