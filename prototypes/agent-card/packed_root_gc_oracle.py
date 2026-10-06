"""Invented packed-root reachability and cursor-retirement oracle.

Run with Python 3. This is a small source model, not a bounded production GC.
"""

import json
import sqlite3
import tempfile
from pathlib import Path

from engine import CursorGap, Engine


def reachable(db: sqlite3.Connection) -> dict[str, set]:
    marked: dict[str, set] = {
        "agent_card_time_nodes": set(),
        "agent_card_presentation_nodes": set(),
        "agent_card_subject_nodes": set(),
        "agent_card_versions": set(),
        "agent_card_local_fact_versions": set(),
    }

    def ordered(node: int | None) -> None:
        if node is None or node in marked["agent_card_presentation_nodes"]:
            return
        row = db.execute(
            "SELECT agent,card_version,local_fact_version,left_id,right_id "
            "FROM agent_card_presentation_nodes WHERE id=?", (node,),
        ).fetchone()
        assert row is not None, ("missing packed ordered root", node)
        marked["agent_card_presentation_nodes"].add(node)
        marked["agent_card_versions"].add((row[0], row[1]))
        marked["agent_card_local_fact_versions"].add((row[0], row[2]))
        ordered(row[3])
        ordered(row[4])

    def time(node: int | None) -> None:
        if node is None or node in marked["agent_card_time_nodes"]:
            return
        row = db.execute(
            "SELECT left_id,right_id,status_roots_json FROM agent_card_time_nodes WHERE id=?",
            (node,),
        ).fetchone()
        assert row is not None, ("missing time root", node)
        marked["agent_card_time_nodes"].add(node)
        for selected in json.loads(row[2]).values():
            ordered(selected)
        time(row[0])
        time(row[1])

    def point(node: int | None) -> None:
        if node is None or node in marked["agent_card_subject_nodes"]:
            return
        row = db.execute(
            "SELECT agent,card_version,left_id,right_id "
            "FROM agent_card_subject_nodes WHERE id=?", (node,),
        ).fetchone()
        assert row is not None, ("missing point root", node)
        marked["agent_card_subject_nodes"].add(node)
        marked["agent_card_versions"].add((row[0], row[1]))
        point(row[2])
        point(row[3])

    for (root,) in db.execute("SELECT time_root_id FROM agent_card_presentation_roots"):
        time(root)
    for (root,) in db.execute("SELECT point_root_id FROM agent_card_roots"):
        point(root)
    # Current local facts must survive even after all old presentation roots retire.
    for pair in db.execute("SELECT agent,version FROM agent_card_local_current"):
        marked["agent_card_local_fact_versions"].add(tuple(pair))
    return marked


def sweep_orphans(db: sqlite3.Connection, marked: dict[str, set]) -> dict[str, int]:
    removed = {}
    for table in ("agent_card_time_nodes", "agent_card_presentation_nodes",
                  "agent_card_subject_nodes"):
        ids = [row[0] for row in db.execute(f"SELECT id FROM {table}")
               if row[0] not in marked[table]]
        if ids:
            db.execute(f"DELETE FROM {table} WHERE id IN ({','.join('?' for _ in ids)})", ids)
        removed[table] = len(ids)
    for table in ("agent_card_versions", "agent_card_local_fact_versions"):
        keys = [tuple(row) for row in db.execute(f"SELECT agent,version FROM {table}")
                if tuple(row) not in marked[table]]
        for key in keys:
            db.execute(f"DELETE FROM {table} WHERE agent=? AND version=?", key)
        removed[table] = len(keys)
    return removed


engine = Engine()
agent = "agent/example/target"
engine.put_agent(agent, "Target", {"name": "Target"}, {"observation": "fresh"})
old = engine.publish()
engine.put_local(agent, {"observation": "changed"}, expires_at=100)
new = engine.publish()
assert engine.detail(old, agent, 100)["observation"] == "fresh"
assert engine.detail(new, agent, 100)["observation"] == "changed"
engine.db.execute("UPDATE agent_card_presentation_roots SET retired_at_ms=10 "
                  "WHERE local_generation=?", (old[3],))
engine.db.commit()
root_count = engine.db.execute(
    "SELECT COUNT(*) FROM agent_card_presentation_roots").fetchone()[0]
engine.db.execute("BEGIN IMMEDIATE")
rolled_back_time = engine.db.execute(
    "INSERT INTO agent_card_time_nodes(depth,status_roots_json) "
    "VALUES(0,'{}') RETURNING id").fetchone()[0]
engine.db.execute(
    "INSERT INTO agent_card_presentation_roots"
    "(epoch,store_index,local_generation,history,time_root_id,created_ms) "
    "VALUES(1,10,999,0,?,0)", (rolled_back_time,))
engine.db.rollback()
assert engine.db.execute(
    "SELECT COUNT(*) FROM agent_card_presentation_roots").fetchone()[0] == root_count
assert engine.detail(old, agent, 100)["observation"] == "fresh"

# An unreferenced packed root and its ordered node must be swept, not retained
# merely because the JSON root is invisible to SQLite foreign keys.
orphan_ordered = engine.db.execute(
    "INSERT INTO agent_card_presentation_nodes"
    "(name,agent,card_version,local_fact_version,final_status,height) "
    "VALUES('Orphan',?,?,?,'waiting',1) RETURNING id", (agent, 1, 1),
).fetchone()[0]
orphan_time = engine.db.execute(
    "INSERT INTO agent_card_time_nodes(depth,status_roots_json) VALUES(0,?) RETURNING id",
    (json.dumps({"waiting": orphan_ordered}),),
).fetchone()[0]
marked = reachable(engine.db)
assert old[2] in marked["agent_card_time_nodes"]
assert new[2] in marked["agent_card_time_nodes"]
assert orphan_time not in marked["agent_card_time_nodes"]
assert orphan_ordered not in marked["agent_card_presentation_nodes"]
assert sweep_orphans(engine.db, marked)["agent_card_time_nodes"] >= 1
assert engine.db.execute("SELECT 1 FROM agent_card_time_nodes WHERE id=?",
                         (orphan_time,)).fetchone() is None
assert engine.detail(old, agent, 100)["observation"] == "fresh"

# TTL starts when a page cursor is issued; this fixture gives the retired root
# its full TTL before deleting the root row and then sweeping reachable nodes.
now_ms, cursor_ttl_ms = 111, 100
engine.db.execute(
    "DELETE FROM agent_card_presentation_roots WHERE retired_at_ms IS NOT NULL "
    "AND retired_at_ms + ? <= ?", (cursor_ttl_ms, now_ms),
)
marked = reachable(engine.db)
assert old[2] not in marked["agent_card_time_nodes"]
assert new[2] in marked["agent_card_time_nodes"]
removed = sweep_orphans(engine.db, marked)
assert removed["agent_card_time_nodes"] > 0
assert engine.detail(new, agent, 100)["observation"] == "changed"
try:
    engine.detail(old, agent, 100)
except CursorGap:
    pass
else:
    raise AssertionError("expired cursor root must reject")

with tempfile.TemporaryDirectory() as directory:
    path = Path(directory, "card.sqlite3")
    reopened = sqlite3.connect(path)
    reopened.row_factory = sqlite3.Row
    engine.db.commit()
    engine.db.backup(reopened)
    engine.db.close()
    engine.db = reopened
    assert engine.detail(new, agent, 100)["observation"] == "changed"
    assert reachable(engine.db)["agent_card_time_nodes"]
    try:
        engine.detail(old, agent, 100)
    except CursorGap:
        pass
    else:
        raise AssertionError("expired cursor must stay rejected after reopen")

print("packed-root JSON reachability, retained cursor, expiry, and reopen passed")
