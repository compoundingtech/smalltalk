"""Draft SQLite persistence for rank-gap summaries; source review only.

This is not wired to Store or the mixed-rank writer. Callers must make leaf,
rank-index, claim/record, and selected-edge edits in one SQLite transaction.
No migration, rebuild, or real lifecycle validation is supplied here.
"""

from __future__ import annotations

from dataclasses import dataclass
import sqlite3


# SQLite's current claims.store_index is an INTEGER. Reject values outside its
# nonnegative signed range; gap magnitudes themselves use decimal TEXT so a
# full u64 recorded position can be compared without narrowing to i64.
DEPTH = 63
WIDTH = 1 << DEPTH

SCHEMA = """
CREATE TABLE reply_gap_leaves (
  child TEXT PRIMARY KEY,
  batch_id TEXT NOT NULL,
  store_index INTEGER NOT NULL,
  legacy_claim_id TEXT NOT NULL,
  recorded_claim_id TEXT NOT NULL,
  legacy_parent TEXT,
  recorded_parent TEXT,
  UNIQUE(batch_id, store_index)
);
CREATE TABLE reply_gap_nodes (
  batch_id TEXT NOT NULL,
  level INTEGER NOT NULL,
  prefix INTEGER NOT NULL,
  leaf_count INTEGER NOT NULL,
  min_gap TEXT NOT NULL,
  max_gap TEXT NOT NULL,
  lazy_gap TEXT NOT NULL,
  PRIMARY KEY(batch_id, level, prefix)
);
"""


@dataclass
class Node:
    count: int
    minimum: int
    maximum: int
    lazy: int


class PersistentGapTree:
    def __init__(self, db: sqlite3.Connection, batch: str) -> None:
        self.db = db
        self.batch = batch
        self.node_reads = 0
        self.node_writes = 0

    def _load(self, level: int, prefix: int) -> Node | None:
        self.node_reads += 1
        row = self.db.execute(
            "SELECT leaf_count,min_gap,max_gap,lazy_gap FROM reply_gap_nodes "
            "WHERE batch_id=? AND level=? AND prefix=?",
            (self.batch, level, prefix),
        ).fetchone()
        return Node(row[0], int(row[1]), int(row[2]), int(row[3])) if row else None

    def _save(self, level: int, prefix: int, node: Node | None) -> None:
        self.node_writes += 1
        if node is None or node.count == 0:
            self.db.execute(
                "DELETE FROM reply_gap_nodes WHERE batch_id=? AND level=? AND prefix=?",
                (self.batch, level, prefix),
            )
            return
        self.db.execute(
            "INSERT INTO reply_gap_nodes VALUES(?,?,?,?,?,?,?) "
            "ON CONFLICT(batch_id,level,prefix) DO UPDATE SET "
            "leaf_count=excluded.leaf_count,min_gap=excluded.min_gap,"
            "max_gap=excluded.max_gap,lazy_gap=excluded.lazy_gap",
            (self.batch, level, prefix, node.count, str(node.minimum),
             str(node.maximum), str(node.lazy)),
        )

    def _apply(self, level: int, prefix: int, node: Node | None,
               delta: int) -> None:
        if node is None or delta == 0:
            return
        node.minimum += delta
        node.maximum += delta
        node.lazy += delta
        self._save(level, prefix, node)

    def _push(self, level: int, prefix: int, node: Node) -> None:
        if node.lazy == 0 or level == DEPTH:
            return
        for side in (0, 1):
            child_prefix = prefix * 2 + side
            child = self._load(level + 1, child_prefix)
            self._apply(level + 1, child_prefix, child, node.lazy)
        node.lazy = 0
        self._save(level, prefix, node)

    def _pull(self, level: int, prefix: int) -> Node | None:
        left = self._load(level + 1, prefix * 2)
        right = self._load(level + 1, prefix * 2 + 1)
        present = [child for child in (left, right) if child is not None]
        if not present:
            self._save(level, prefix, None)
            return None
        node = Node(sum(child.count for child in present),
                    min(child.minimum for child in present),
                    max(child.maximum for child in present), 0)
        self._save(level, prefix, node)
        return node

    def _set(self, level: int, prefix: int, index: int,
             gap: int | None) -> None:
        if level == DEPTH:
            self._save(level, prefix, Node(1, gap, gap, 0)
                       if gap is not None else None)
            return
        node = self._load(level, prefix)
        if node is not None:
            self._push(level, prefix, node)
        side = (index >> (DEPTH - level - 1)) & 1
        self._set(level + 1, prefix * 2 + side, index, gap)
        self._pull(level, prefix)

    def replace_head(self, *, child: str, store_index: int | None,
                     gap: int | None = None,
                     legacy_claim_id: str = '', recorded_claim_id: str = '',
                     legacy_parent: str | None = None,
                     recorded_parent: str | None = None) -> None:
        """OLD removal and NEW insertion at one publication cut.

        The caller computes whether the two lane heads share a canonical
        prefix and supplies gap only then. A missing NEW index removes the
        old leaf. A caller must refresh this direct child after the operation.
        """
        old = self.db.execute(
            "SELECT batch_id,store_index FROM reply_gap_leaves WHERE child=?",
            (child,),
        ).fetchone()
        if old is not None:
            old_batch, old_index = old
            PersistentGapTree(self.db, old_batch)._set(0, 0, old_index, None)
            self.db.execute("DELETE FROM reply_gap_leaves WHERE child=?", (child,))
        if store_index is None:
            assert gap is None
            return
        assert gap is not None and 0 < store_index < WIDTH
        self.db.execute(
            "INSERT INTO reply_gap_leaves VALUES(?,?,?,?,?,?,?)",
            (child, self.batch, store_index, legacy_claim_id,
             recorded_claim_id, legacy_parent, recorded_parent),
        )
        self._set(0, 0, store_index, gap)

    def shift_after(self, index: int, delta: int) -> list[str]:
        """Persist one COUNT-rank shift, returning potential changed children.

        Only unit shifts are valid. The caller applies this and rank-index
        mutation together, then refreshes returned children before commit.
        """
        assert 0 <= index < WIDTH and delta in (-1, 1)
        keys: list[int] = []

        def visit(level: int, prefix: int, low: int, high: int) -> None:
            if high <= index + 1:
                return
            node = self._load(level, prefix)
            if node is None:
                return
            if low > index and (node.maximum < -1 or node.minimum > 1):
                self._apply(level, prefix, node, delta)
                return
            if level == DEPTH:
                keys.append(low)
                self._apply(level, prefix, node, delta)
                return
            self._push(level, prefix, node)
            middle = (low + high) // 2
            visit(level + 1, prefix * 2, low, middle)
            visit(level + 1, prefix * 2 + 1, middle, high)
            self._pull(level, prefix)

        visit(0, 0, 0, WIDTH)
        return [self.db.execute(
            "SELECT child FROM reply_gap_leaves WHERE batch_id=? AND store_index=?",
            (self.batch, key),
        ).fetchone()[0] for key in keys]

    def gap_at(self, index: int) -> int | None:
        """Read through lazy ancestors without mutating persisted rows."""
        assert 0 <= index < WIDTH
        prefix = 0
        carried = 0
        for level in range(DEPTH + 1):
            node = self._load(level, prefix)
            if node is None:
                return None
            if level == DEPTH:
                return node.minimum + carried
            carried += node.lazy
            prefix = prefix * 2 + ((index >> (DEPTH - level - 1)) & 1)
        raise AssertionError('unreachable')
