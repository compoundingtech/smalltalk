"""Sparse range-add tree for mixed reply-lane rank crossings.

This is an invented in-memory ordering oracle. Node visits are algorithmic
counts, not SQLite VM steps or a production writer-cost measurement.
"""

from __future__ import annotations

from dataclasses import dataclass
import random


WIDTH = 1 << 16
INF = 1 << 60


@dataclass
class Node:
    left: Node | None = None
    right: Node | None = None
    minimum: int = INF
    maximum: int = -INF
    lazy: int = 0
    count: int = 0


class GapTree:
    """One leaf per active legacy head, keyed by its claim store index.

    A leaf's gap is current legacy COUNT rank minus its recorded-lane position.
    Only heads whose canonical prefix ties the recorded head need a leaf.
    Other prefixes and direct claim/record changes are handled separately.
    """

    def __init__(self) -> None:
        self.root = Node()
        self.visits = 0

    @staticmethod
    def _pull(node: Node) -> None:
        children = [part for part in (node.left, node.right)
                    if part is not None and part.count]
        node.count = sum(part.count for part in children)
        node.minimum = min((part.minimum for part in children), default=INF)
        node.maximum = max((part.maximum for part in children), default=-INF)

    @staticmethod
    def _apply(node: Node, delta: int) -> None:
        if node.count:
            node.minimum += delta
            node.maximum += delta
            node.lazy += delta

    def _push(self, node: Node) -> None:
        if not node.lazy:
            return
        for child in (node.left, node.right):
            if child is not None:
                self._apply(child, node.lazy)
        node.lazy = 0

    def set(self, key: int, gap: int | None) -> None:
        assert 0 <= key < WIDTH

        def visit(node: Node, low: int, high: int) -> None:
            self.visits += 1
            if high - low == 1:
                node.count = int(gap is not None)
                node.minimum = gap if gap is not None else INF
                node.maximum = gap if gap is not None else -INF
                node.lazy = 0
                return
            self._push(node)
            middle = (low + high) // 2
            name = 'left' if key < middle else 'right'
            child = getattr(node, name)
            if child is None:
                child = Node()
                setattr(node, name, child)
            visit(child, low, middle) if key < middle else visit(child, middle, high)
            self._pull(node)

        visit(self.root, 0, WIDTH)

    def shift_after(self, index: int, delta: int) -> tuple[list[int], int]:
        """Shift ranks and report heads whose rank/recorded order may cross.

        For a unit shift, old gaps in {-1, 0, 1} are conservative candidates.
        A canonical claim-id tie may make some of these non-changing.
        """
        assert delta in (-1, 1)
        before = self.visits
        candidates: list[int] = []

        def visit(node: Node | None, low: int, high: int) -> None:
            if node is None or not node.count or high <= index + 1:
                return
            self.visits += 1
            if low > index and (node.maximum < -1 or node.minimum > 1):
                self._apply(node, delta)
                return
            if high - low == 1:
                candidates.append(low)
                self._apply(node, delta)
                return
            self._push(node)
            middle = (low + high) // 2
            visit(node.left, low, middle)
            visit(node.right, middle, high)
            self._pull(node)

        visit(self.root, 0, WIDTH)
        return candidates, self.visits - before

    def value(self, key: int) -> int | None:
        node = self.root
        low, high = 0, WIDTH
        while node.count and high - low > 1:
            self._push(node)
            middle = (low + high) // 2
            if key < middle:
                node, high = node.left, middle
            else:
                node, low = node.right, middle
            if node is None:
                return None
        return node.minimum if node.count else None


def fixture(size: int, recorded_position) -> dict[str, int]:
    tree = GapTree()
    children = {}
    for number in range(size):
        index = 2 * number + 3
        rank = index - 1
        gap = rank - recorded_position(number)
        tree.set(index, gap)
        children[index] = gap
    candidates, visits = tree.shift_after(1, -1)
    expected = {index for index, gap in children.items()
                if -1 <= gap <= 1}
    assert set(candidates) == expected, (size, candidates, expected)
    # The recorded claim wins a position tie by claim ID. Compare both
    # parents independently of the range tree's candidate enumeration.
    winner = lambda gap: 'legacy' if (gap, 'a-legacy') > (0, 'z-recorded') else 'recorded'
    changed = sum(winner(gap) != winner(gap - 1) for gap in children.values())
    for index, gap in children.items():
        assert tree.value(index) == gap - 1, (index, gap)
    return {'children': size, 'candidates': len(candidates),
            'changed': changed, 'range_node_visits': visits}


def mutations() -> None:
    randomizer = random.Random(719)
    tree = GapTree()
    expected: dict[int, int] = {}
    for _ in range(500):
        index = randomizer.randrange(1, 200)
        choice = randomizer.randrange(3)
        if choice == 0:
            gap = randomizer.randrange(-4, 5)
            tree.set(index, gap)
            expected[index] = gap
        elif choice == 1:
            tree.set(index, None)
            expected.pop(index, None)
        else:
            delta = randomizer.choice((-1, 1))
            candidates, _ = tree.shift_after(index, delta)
            assert set(candidates) == {key for key, gap in expected.items()
                                       if key > index and -1 <= gap <= 1}
            for key in expected:
                if key > index:
                    expected[key] += delta
        assert all(tree.value(key) == gap for key, gap in expected.items())
    print('random set/delete/range shift oracle passed', len(expected))


if __name__ == '__main__':
    mutations()
    for size in (10, 100):
        far = fixture(size, lambda _: 100_000)
        crossing = fixture(size, lambda number: 2 * number + 1)
        assert (far['candidates'], far['changed']) == (0, 0)
        assert (crossing['candidates'], crossing['changed']) == (size, size)
        print('far recorded head', far)
        print('all crossing', crossing)
