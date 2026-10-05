"""Executable invented tree/page/point/status and work-counter checks.

Run with `python3 prototypes/agent-card/engine_oracle.py`. This does not call
Store or establish production latency.
"""

from engine import END, CursorGap, Engine


def check(fleet: int, revisions: int) -> dict:
    engine = Engine()
    names = [f"agent/example/{index:04}" for index in range(fleet)]
    for index, agent in enumerate(names):
        engine.put_agent(agent, f"Agent {index:04}", {"name": f"Agent {index:04}"},
                         {"observation": "fresh"}, END)
    first = engine.publish()
    before = engine.snapshot_cost()
    page = engine.page(first, 50, "running", None, 5)
    first_read = tuple(a-b for a, b in zip(engine.snapshot_cost(), before))
    assert [row["id"] for row in page] == names[:6]
    assert engine.point(engine.point_root, names[-1]) == {"name": f"Agent {fleet-1:04}"}
    assert engine.detail(first, names[-1], 50)["id"] == names[-1]

    target = names[1]
    old_cursor = (first, 50, ("Agent 0000", names[0]))
    before = engine.snapshot_cost()
    per_revision = []
    for revision in range(revisions):
        revision_before = engine.snapshot_cost()
        engine.put_local(target, {"observation": f"revision-{revision}"}, 100)
        per_revision.append(tuple(a-b for a, b in zip(engine.snapshot_cost(), revision_before)))
    new_root = engine.publish()
    write = tuple(a-b for a, b in zip(engine.snapshot_cost(), before))
    before = engine.snapshot_cost()
    old_page_two = engine.page(old_cursor[0], old_cursor[1], "running", old_cursor[2], 1)
    assert old_page_two[0]["id"] == target
    assert old_page_two[0]["observation"] == "fresh"
    assert old_page_two[0]["status"] == "running"
    assert engine.detail(old_cursor[0], target, 50)["observation"] == "fresh"
    new_page_two = engine.page(new_root, 50, "running", old_cursor[2], 1)
    assert new_page_two[0]["id"] == target
    assert new_page_two[0]["observation"] == f"revision-{revisions-1}"
    assert engine.page(new_root, 100, "waiting", None, 1)[0]["id"] == target
    assert engine.detail(new_root, target, 100)["status"] == "waiting"
    assert all(row["id"] != target for row in engine.page(new_root, 100, "running", None, fleet))
    assert len(engine.page(new_root, 100, "*", None, fleet)) == fleet
    follow_read = tuple(a-b for a, b in zip(engine.snapshot_cost(), before))
    assert engine.detail(new_root, target, 99)["status"] == "running"
    assert engine.detail(old_cursor[0], target, 100)["status"] == "running"

    before = engine.snapshot_cost()
    engine.put_local(target, {"observation": f"revision-{revisions-1}"}, 100)
    assert engine.snapshot_cost() == before, "no-op local fact must touch zero rows"
    engine.put_local(target, {"observation": f"revision-{revisions-1}"}, END)
    extended_root = engine.publish()
    assert engine.detail(extended_root, target, 100)["status"] == "running"
    assert engine.detail(new_root, target, 100)["status"] == "waiting"
    assert engine.verify_avl("point", engine.point_root) >= 1
    assert engine.verify_time_tree(old_cursor[0][2]) >= 1
    assert engine.verify_time_tree(new_root[2]) >= 1
    engine.fence_epoch(10)
    try:
        engine.page(old_cursor[0], old_cursor[1], "running", old_cursor[2], 1)
    except CursorGap:
        pass
    else:
        raise AssertionError("old cursor must be rejected at an invalid canonical cut")
    result = {"fleet": fleet, "revisions": revisions,
              "first_page_statements_nodes_bytes": first_read,
              "related_write_statements_nodes_bytes": write,
              "first_revision_statements_nodes_bytes": per_revision[0],
              "last_revision_statements_nodes_bytes": per_revision[-1],
              "old_new_page_and_status_statements_nodes_bytes": follow_read}
    print(result)
    return result


small_short = check(8, 1)
large_short = check(32, 1)
small_long = check(8, 8)
large_long = check(32, 8)
assert large_short["first_page_statements_nodes_bytes"][0] < 4 * small_short["first_page_statements_nodes_bytes"][0]
assert large_long["first_page_statements_nodes_bytes"][0] < 4 * small_long["first_page_statements_nodes_bytes"][0]
print("invented AVL/time traversal, cursor, status, point, and no-op checks passed")
