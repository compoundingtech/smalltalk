"""Executable SQLite storage/tree spike for keyed agent cards.

Uses immutable AVL nodes and a persistent u128 time segment tree. It is not
wired to Store, so actual Store JSON parity and production cost remain open.
"""

from __future__ import annotations

import heapq
import json
import sqlite3
from pathlib import Path
from typing import Iterator

END = 1 << 128


class CursorGap(Exception):
    pass


class Engine:
    def __init__(self) -> None:
        self.db = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.execute("CREATE TABLE desired(subject TEXT,kind TEXT,owner_generation TEXT)")
        self.db.executescript(Path(__file__).with_name("prototype.sql").read_text())
        self.db.execute("INSERT INTO agent_card_epochs(epoch,source_digest) VALUES(1,'invented')")
        self.statements = 0
        self.nodes = 0
        self.bytes_written = 0
        self.card_versions: dict[str, int] = {}
        self.local_versions: dict[str, int] = {}
        self.cards: dict[str, dict] = {}
        self.facts: dict[str, dict] = {}
        self.names: dict[str, str] = {}
        self.presentation_root: int | None = None
        self.point_root: int | None = None
        self.local_generation = 0
        self.intervals: dict[str, list[tuple[int, int, str, int]]] = {}

    def sql(self, query: str, args: tuple = ()) -> sqlite3.Cursor:
        self.statements += 1
        self.bytes_written += len(query) + sum(len(str(value)) for value in args)
        return self.db.execute(query, args)

    def snapshot_cost(self) -> tuple[int, int, int]:
        return self.statements, self.nodes, self.bytes_written

    def _table(self, kind: str) -> str:
        return "agent_card_subject_nodes" if kind == "point" else "agent_card_presentation_nodes"

    def _row(self, kind: str, node: int | None) -> sqlite3.Row | None:
        if node is None:
            return None
        return self.sql(f"SELECT * FROM {self._table(kind)} WHERE id=?", (node,)).fetchone()

    def _height(self, kind: str, node: int | None) -> int:
        row = self._row(kind, node)
        return 0 if row is None else row["height"]

    def _key(self, kind: str, row: sqlite3.Row) -> tuple[str, ...]:
        return (row["agent"],) if kind == "point" else (row["name"], row["agent"])

    def _payload(self, kind: str, row: sqlite3.Row) -> tuple:
        return ((row["card_version"],) if kind == "point" else
                (row["card_version"], row["local_fact_version"], row["final_status"]))

    def _make(self, kind: str, key: tuple[str, ...], payload: tuple,
              left: int | None, right: int | None) -> int:
        height = 1 + max(self._height(kind, left), self._height(kind, right))
        if kind == "point":
            result = self.sql(
                "INSERT INTO agent_card_subject_nodes"
                "(agent,card_version,height,left_id,right_id) VALUES(?,?,?,?,?) RETURNING id",
                (key[0], payload[0], height, left, right),
            )
        else:
            result = self.sql(
                "INSERT INTO agent_card_presentation_nodes"
                "(name,agent,card_version,local_fact_version,final_status,height,left_id,right_id) "
                "VALUES(?,?,?,?,?,?,?,?) RETURNING id",
                (key[0], key[1], *payload, height, left, right),
            )
        self.nodes += 1
        return result.fetchone()[0]

    def _rotate_left(self, kind: str, node: int) -> int:
        parent = self._row(kind, node)
        child = self._row(kind, parent["right_id"])
        lower = self._make(kind, self._key(kind, parent), self._payload(kind, parent),
                           parent["left_id"], child["left_id"])
        return self._make(kind, self._key(kind, child), self._payload(kind, child),
                          lower, child["right_id"])

    def _rotate_right(self, kind: str, node: int) -> int:
        parent = self._row(kind, node)
        child = self._row(kind, parent["left_id"])
        lower = self._make(kind, self._key(kind, parent), self._payload(kind, parent),
                           child["right_id"], parent["right_id"])
        return self._make(kind, self._key(kind, child), self._payload(kind, child),
                          child["left_id"], lower)

    def _balance(self, kind: str, node: int) -> int:
        row = self._row(kind, node)
        left_height = self._height(kind, row["left_id"])
        right_height = self._height(kind, row["right_id"])
        if left_height - right_height > 1:
            left = self._row(kind, row["left_id"])
            if self._height(kind, left["left_id"]) < self._height(kind, left["right_id"]):
                new_left = self._rotate_left(kind, row["left_id"])
                node = self._make(kind, self._key(kind, row), self._payload(kind, row),
                                  new_left, row["right_id"])
            return self._rotate_right(kind, node)
        if right_height - left_height > 1:
            right = self._row(kind, row["right_id"])
            if self._height(kind, right["right_id"]) < self._height(kind, right["left_id"]):
                new_right = self._rotate_right(kind, row["right_id"])
                node = self._make(kind, self._key(kind, row), self._payload(kind, row),
                                  row["left_id"], new_right)
            return self._rotate_left(kind, node)
        return node

    def put(self, kind: str, root: int | None, key: tuple[str, ...], payload: tuple) -> int:
        if root is None:
            return self._make(kind, key, payload, None, None)
        row = self._row(kind, root)
        old = self._key(kind, row)
        if key < old:
            left = self.put(kind, row["left_id"], key, payload)
            node = self._make(kind, old, self._payload(kind, row), left, row["right_id"])
        elif key > old:
            right = self.put(kind, row["right_id"], key, payload)
            node = self._make(kind, old, self._payload(kind, row), row["left_id"], right)
        else:
            node = self._make(kind, key, payload, row["left_id"], row["right_id"])
        return self._balance(kind, node)

    def _pop_min(self, kind: str, root: int) -> tuple[sqlite3.Row, int | None]:
        row = self._row(kind, root)
        if row["left_id"] is None:
            return row, row["right_id"]
        minimum, left = self._pop_min(kind, row["left_id"])
        node = self._make(kind, self._key(kind, row), self._payload(kind, row),
                          left, row["right_id"])
        return minimum, self._balance(kind, node)

    def drop(self, kind: str, root: int | None, key: tuple[str, ...]) -> int | None:
        if root is None:
            return None
        row = self._row(kind, root)
        old = self._key(kind, row)
        if key < old:
            left = self.drop(kind, row["left_id"], key)
            node = self._make(kind, old, self._payload(kind, row), left, row["right_id"])
        elif key > old:
            right = self.drop(kind, row["right_id"], key)
            node = self._make(kind, old, self._payload(kind, row), row["left_id"], right)
        elif row["left_id"] is None:
            return row["right_id"]
        elif row["right_id"] is None:
            return row["left_id"]
        else:
            successor, right = self._pop_min(kind, row["right_id"])
            node = self._make(kind, self._key(kind, successor), self._payload(kind, successor),
                              row["left_id"], right)
        return self._balance(kind, node)

    def point(self, root: int | None, agent: str) -> dict | None:
        while root is not None:
            row = self._row("point", root)
            if agent == row["agent"]:
                card = self.sql(
                    "SELECT card_json FROM agent_card_versions WHERE agent=? AND version=?",
                    (agent, row["card_version"]),
                ).fetchone()
                return json.loads(card[0])
            root = row["left_id"] if agent < row["agent"] else row["right_id"]
        return None

    def _find(self, kind: str, root: int | None,
              key: tuple[str, ...]) -> sqlite3.Row | None:
        while root is not None:
            row = self._row(kind, root)
            found = self._key(kind, row)
            if key == found:
                return row
            root = row["left_id"] if key < found else row["right_id"]
        return None

    def _ordered_after(self, root: int | None,
                       after: tuple[str, str] | None) -> Iterator[sqlite3.Row]:
        stack: list[sqlite3.Row] = []
        while root is not None or stack:
            while root is not None:
                row = self._row("ordered", root)
                if after is not None and self._key("ordered", row) <= after:
                    root = row["right_id"]
                else:
                    stack.append(row)
                    root = row["left_id"]
            if stack:
                row = stack.pop()
                yield row
                root = row["right_id"]

    def verify_avl(self, kind: str, root: int | None) -> int:
        seen: dict[int, tuple[tuple[str, ...], tuple[str, ...], int]] = {}

        def check(node: int | None):
            if node is None:
                return None
            if node in seen:
                return seen[node]
            row = self._row(kind, node)
            key = self._key(kind, row)
            left = check(row["left_id"])
            right = check(row["right_id"])
            assert left is None or left[1] < key
            assert right is None or key < right[0]
            left_height = 0 if left is None else left[2]
            right_height = 0 if right is None else right[2]
            assert abs(left_height - right_height) <= 1
            assert row["height"] == max(left_height, right_height) + 1
            result = (key if left is None else left[0],
                      key if right is None else right[1], row["height"])
            seen[node] = result
            return result

        result = check(root)
        return 0 if result is None else result[2]

    def verify_time_tree(self, root: int | None) -> int:
        seen: set[int] = set()

        def check(node: int | None, depth: int):
            if node is None or node in seen:
                return
            seen.add(node)
            row = self.sql("SELECT depth,left_id,right_id,status_roots_json FROM agent_card_time_nodes "
                           "WHERE id=?", (node,)).fetchone()
            assert row["depth"] == depth
            for status_root in json.loads(row["status_roots_json"]).values():
                self.verify_avl("ordered", status_root)
            check(row["left_id"], depth + 1)
            check(row["right_id"], depth + 1)

        check(root, 0)
        return len(seen)

    def _time(self, node: int | None) -> tuple[int | None, int | None, dict[str, int]]:
        if node is None:
            return None, None, {}
        row = self.sql("SELECT left_id,right_id,status_roots_json FROM agent_card_time_nodes WHERE id=?",
                       (node,)).fetchone()
        return row[0], row[1], json.loads(row[2])

    def _time_make(self, depth: int, left: int | None, right: int | None,
                   roots: dict[str, int]) -> int:
        result = self.sql(
            "INSERT INTO agent_card_time_nodes(depth,left_id,right_id,status_roots_json) "
            "VALUES(?,?,?,?) RETURNING id",
            (depth, left, right, json.dumps(roots, sort_keys=True)),
        )
        node = result.fetchone()[0]
        self.nodes += 1
        return node

    def _time_interval(self, node: int | None, depth: int, lo: int, hi: int,
                       start: int, end: int,
                       actions: tuple[tuple[str, tuple[str, str], tuple, bool], ...]) -> int | None:
        if start >= hi or end <= lo:
            return node
        left, right, roots = self._time(node)
        if start <= lo and hi <= end:
            for status, key, payload, remove in actions:
                old_root = roots.get(status)
                new_root = (self.drop("ordered", old_root, key) if remove else
                            self.put("ordered", old_root, key, payload))
                if new_root is None:
                    roots.pop(status, None)
                else:
                    roots[status] = new_root
        else:
            middle = (lo + hi) // 2
            left = self._time_interval(left, depth + 1, lo, middle, start, end,
                                       actions)
            right = self._time_interval(right, depth + 1, middle, hi, start, end,
                                        actions)
        if not roots and left is None and right is None:
            return None
        return self._time_make(depth, left, right, roots)

    def _interval(self, root: int | None, start: int, end: int, status: str,
                  key: tuple[str, str], payload: tuple, remove: bool) -> int | None:
        if start == end:
            return root
        return self._time_interval(root, 0, 0, END, start, end,
                                   (("*", key, payload, remove),
                                    (status, key, payload, remove)))

    def put_agent(self, agent: str, name: str, card: dict,
                  facts: dict, expires_at: int = END) -> None:
        assert 0 <= expires_at <= END
        old = self.intervals.get(agent, [])
        if (self.cards.get(agent) == card and self.facts.get(agent) == facts
                and self.names.get(agent) == name
                and (not old or old[0][1] == expires_at)):
            return
        old_key = (self.names.get(agent, name), agent)
        key = (name, agent)
        card_changed = self.cards.get(agent) != card
        membership_changed = (not old or card_changed or old_key != key
                              or old[0][1] != expires_at)
        if membership_changed:
            for start, end, status, fact_version in old:
                payload = (self.card_versions[agent], fact_version, status)
                self.presentation_root = self._interval(self.presentation_root, start, end,
                                                         status, old_key, payload, True)
        card_version = self.card_versions.get(agent, 0) + int(card_changed)
        fact_version = self.local_versions.get(agent, 0) + 1
        self.local_generation += 1
        if card_changed:
            self.sql("INSERT INTO agent_card_versions(agent,version,card_json) VALUES(?,?,?)",
                     (agent, card_version, json.dumps(card, sort_keys=True)))
        self.sql("INSERT INTO agent_card_local_fact_versions"
                 "(agent,version,generation,facts_json) VALUES(?,?,?,?)",
                 (agent, fact_version, self.local_generation,
                  json.dumps(facts, sort_keys=True)))
        deadline = None if expires_at == END else expires_at.to_bytes(16, "big")
        self.sql("INSERT OR REPLACE INTO agent_card_local_current"
                 "(agent,version,next_deadline_ms) VALUES(?,?,?)",
                 (agent, fact_version, deadline))
        if card_changed:
            self.point_root = self.put("point", self.point_root, (agent,), (card_version,))
        self.card_versions[agent] = card_version
        self.local_versions[agent] = fact_version
        self.cards[agent] = card.copy()
        self.facts[agent] = facts.copy()
        self.names[agent] = name
        intervals = [(0, expires_at, "running", fact_version)]
        if expires_at < END:
            intervals.append((expires_at, END, "waiting", fact_version))
        if membership_changed:
            for start, end, status, version in intervals:
                payload = (card_version, version, status)
                self.presentation_root = self._interval(self.presentation_root, start, end,
                                                         status, key, payload, False)
            self.intervals[agent] = intervals

    def put_local(self, agent: str, facts: dict, expires_at: int = END) -> None:
        self.put_agent(agent, self.names[agent], self.cards[agent], facts, expires_at)

    def publish(self, cut: int = 10) -> tuple[int, int, int, int, int]:
        result = self.sql(
            "INSERT INTO agent_card_presentation_roots"
            "(epoch,store_index,local_generation,history,time_root_id,created_ms) "
            "VALUES(1,?,?,0,?,0) RETURNING time_root_id",
            (cut, self.local_generation, self.presentation_root),
        )
        self.sql("INSERT OR IGNORE INTO agent_card_roots"
                 "(epoch,store_index,history,status,point_root_id) VALUES(1,?,0,'*',?)",
                 (cut, self.point_root))
        return 1, cut, result.fetchone()[0], self.local_generation, self.point_root

    def fence_epoch(self, first_changed_cut: int) -> None:
        self.sql("UPDATE agent_card_epochs SET valid_through_store_index=? WHERE epoch=1",
                 (first_changed_cut - 1,))

    def _checked_root(self, snapshot: tuple[int, int, int, int, int]) -> tuple[int, int, int]:
        epoch, cut, root, generation, point_root = snapshot
        validity = self.sql(
            "SELECT valid_through_store_index FROM agent_card_epochs WHERE epoch=?",
            (epoch,),
        ).fetchone()
        if validity is None or (validity[0] is not None and cut > validity[0]):
            raise CursorGap(f"epoch {epoch} no longer covers source cut {cut}")
        return root, generation, point_root

    def _roots_at(self, root: int, at: int, status: str) -> list[int]:
        roots: list[int] = []
        lo, hi = 0, END
        node: int | None = root
        while node is not None:
            left, right, selectors = self._time(node)
            if status in selectors:
                roots.append(selectors[status])
            middle = (lo + hi) // 2
            if at < middle:
                node, hi = left, middle
            else:
                node, lo = right, middle
        return roots

    def detail(self, snapshot: tuple[int, int, int, int, int], agent: str,
               at: int) -> dict | None:
        assert 0 <= at < END
        root, generation, point_root = self._checked_root(snapshot)
        card = self.point(point_root, agent)
        if card is None:
            return None
        key = (card["name"], agent)
        for ordered in self._roots_at(root, at, "*"):
            row = self._find("ordered", ordered, key)
            if row is None:
                continue
            facts = self.sql(
                "SELECT facts_json FROM agent_card_local_fact_versions "
                "WHERE agent=? AND generation<=? ORDER BY generation DESC LIMIT 1",
                (agent, generation),
            ).fetchone()
            return {**card, **json.loads(facts[0]), "id": agent,
                    "status": row["final_status"]}
        return None

    def page(self, snapshot: tuple[int, int, int, int, int], at: int,
             status: str, after: tuple[str, str] | None,
             limit: int) -> list[dict]:
        assert 0 <= at < END
        root, generation, _ = self._checked_root(snapshot)
        roots = self._roots_at(root, at, status)
        streams = [self._ordered_after(node, after) for node in roots]
        merged = heapq.merge(*streams, key=lambda row: (row["name"], row["agent"]))
        answer = []
        for row in merged:
            card = self.sql(
                "SELECT card_json FROM agent_card_versions WHERE agent=? AND version=?",
                (row["agent"], row["card_version"]),
            ).fetchone()
            facts = self.sql(
                "SELECT facts_json FROM agent_card_local_fact_versions "
                "WHERE agent=? AND generation<=? ORDER BY generation DESC LIMIT 1",
                (row["agent"], generation),
            ).fetchone()
            answer.append({**json.loads(card[0]), **json.loads(facts[0]),
                           "id": row["agent"], "status": row["final_status"]})
            if len(answer) >= limit + 1:
                break
        return answer
