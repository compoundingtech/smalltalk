"""Invented schema oracle for an old page cursor across a local fact change.

This checks immutable references and prior status membership, not AVL balancing,
interval traversal, daemon behavior, or latency. Run with Python 3.
"""

import json
import sqlite3
from pathlib import Path


database = sqlite3.connect(":memory:")
database.execute("PRAGMA foreign_keys=ON")
database.execute("CREATE TABLE desired(subject TEXT,kind TEXT,owner_generation TEXT)")
database.executescript(Path(__file__).with_name("prototype.sql").read_text())
database.execute("INSERT INTO agent_card_epochs(epoch,source_digest) VALUES(1,'invented-cut')")
anchor = "agent/example/anchor"
target = "agent/example/target"
for agent, name in [(anchor, "Anchor"), (target, "Target")]:
    database.execute(
        "INSERT INTO agent_card_versions(agent,version,card_json) VALUES(?,?,?)",
        (agent, 1, json.dumps({"name": name, "state": "running"})),
    )


def fact(agent: str, version: int, observation: str) -> None:
    database.execute(
        "INSERT INTO agent_card_local_fact_versions"
        "(agent,version,generation,facts_json) VALUES(?,?,?,?)",
        (agent, version, version, json.dumps({"observation": observation})),
    )
    database.execute(
        "INSERT OR REPLACE INTO agent_card_local_current(agent,version) VALUES(?,?)",
        (agent, version),
    )


def node(agent: str, name: str, version: int, status: str, right=None) -> int:
    return database.execute(
        "INSERT INTO agent_card_presentation_nodes"
        "(name,agent,card_version,local_fact_version,final_status,height,right_id) "
        "VALUES(?,?,?,?,?,?,?) RETURNING id",
        (name, agent, 1, version, status, 2 if right else 1, right),
    ).fetchone()[0]


def root(generation: int, status_roots: dict[str, int]) -> int:
    time_node = database.execute(
        "INSERT INTO agent_card_time_nodes(depth,status_roots_json) VALUES(0,?) RETURNING id",
        (json.dumps(status_roots, sort_keys=True),),
    ).fetchone()[0]
    database.execute(
        "INSERT INTO agent_card_presentation_roots"
        "(epoch,store_index,local_generation,history,time_root_id,created_ms) "
        "VALUES(1,10,?,0,?,0)",
        (generation, time_node),
    )
    return time_node


fact(anchor, 1, "fresh")
fact(target, 1, "fresh")
old_target = node(target, "Target", 1, "running")
old_anchor = node(anchor, "Anchor", 1, "running", right=old_target)
old_root = root(1, {"running": old_anchor})

# Page one is Anchor. Hold the old root and last key, then publish a local
# update that removes Target from the new running-status root.
assert database.execute(
    "SELECT agent FROM agent_card_presentation_nodes WHERE id=?", (old_anchor,)
).fetchone() == (anchor,)
fact(target, 2, "stale")
new_target = node(target, "Target", 2, "waiting")
new_anchor = node(anchor, "Anchor", 1, "running")
new_root = root(2, {"running": new_anchor, "waiting": new_target})


def page_after_anchor(time_root: int, status: str):
    ordered_root = json.loads(database.execute(
        "SELECT status_roots_json FROM agent_card_time_nodes WHERE id=?",
        (time_root,),
    ).fetchone()[0])[status]
    next_id = database.execute(
        "SELECT right_id FROM agent_card_presentation_nodes WHERE id=?",
        (ordered_root,),
    ).fetchone()[0]
    if next_id is None:
        return None
    row = database.execute(
        "SELECT p.agent,p.final_status,f.facts_json "
        "FROM agent_card_presentation_nodes AS p "
        "JOIN agent_card_local_fact_versions AS f "
        "ON f.agent=p.agent AND f.version=p.local_fact_version WHERE p.id=?",
        (next_id,),
    ).fetchone()
    return row[0], row[1], json.loads(row[2])["observation"]


assert database.execute(
    "SELECT version FROM agent_card_local_current WHERE agent=?", (target,)
).fetchone() == (2,)
assert page_after_anchor(old_root, "running") == (target, "running", "fresh")
assert page_after_anchor(new_root, "running") is None
new_waiting = json.loads(database.execute(
    "SELECT status_roots_json FROM agent_card_time_nodes WHERE id=?",
    (new_root,),
).fetchone()[0])["waiting"]
assert database.execute(
    "SELECT final_status,local_fact_version FROM agent_card_presentation_nodes WHERE id=?",
    (new_waiting,),
).fetchone() == ("waiting", 2)

# An unchanged agent survives a canonical epoch fork through the immutable
# subject root. The prior epoch's changed cut is fenced for old cursors.
point = database.execute(
    "INSERT INTO agent_card_subject_nodes(agent,card_version,height) "
    "VALUES(?,?,1) RETURNING id",
    (target, 1),
).fetchone()[0]
database.execute(
    "INSERT INTO agent_card_roots"
    "(epoch,store_index,history,status,point_root_id) VALUES(1,10,0,'*',?)",
    (point,),
)
database.execute(
    "UPDATE agent_card_epochs SET valid_through_store_index=9 WHERE epoch=1"
)
database.execute(
    "INSERT INTO agent_card_epochs(epoch,source_digest) VALUES(2,'repaired-cut')"
)
database.execute(
    "INSERT INTO agent_card_roots"
    "(epoch,store_index,history,status,point_root_id) VALUES(2,10,0,'*',?)",
    (point,),
)
queries = Path(__file__).with_name("candidate_queries.sql").read_text()
start = queries.index("WITH RECURSIVE seek")
point_query = queries[start : queries.index(";", start)]
assert database.execute(point_query, (2, 0, 10, target)).fetchone() == (
    json.dumps({"name": "Target", "state": "running"}),
)
assert database.execute(
    "SELECT valid_through_store_index FROM agent_card_epochs WHERE epoch=1"
).fetchone() == (9,)
print("old page two keeps Target running/fresh; epoch fork keeps unchanged point card")
