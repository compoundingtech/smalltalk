"""Await daemon events in tests instead of polling snapshots.

    events = Events(socket, alive=lambda: daemon.poll() is None or fail("daemon exited"))
    mark = events.mark()                     # before the action under test
    ...act...
    mark.wait(claim("harness.diagnostic", subject, status="blocked"), 45, "the seat never blocked")

A mark is a cursor in the daemon's event log (`/v1/events/page`), taken from `/v1/health`'s
`store_index` before the action. Every claim the daemon accepts after it is still in the log when
the test looks, so a state that lasted 100 ms is found as surely as one that lasts. A snapshot
poll can miss it. Waits are long polls, so no interval is chosen here and no process is spawned:
the daemon answers when it appends an event. The ceiling only bounds failure; nothing succeeds
by timing out.

Failures are immediate where waiting cannot help: a cursor outside the retained log (410
cursor-gap) is a test bug, and a daemon that exits or drops the connection is not coming back.
At the ceiling the failure names the predicate and prints the last 20 events seen.
"""
from collections import deque
import http.client
import json
import socket
import time
from urllib.parse import quote

LONG_POLL_MS = 30_000
SEEN = 20


class EventWaitFailed(Exception):
    pass


class _UnixConnection(http.client.HTTPConnection):
    def __init__(self, path, timeout):
        super().__init__("localhost", timeout=timeout)
        self._path = path

    def connect(self):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(self.timeout)
        sock.connect(str(self._path))
        self.sock = sock


def _value(payload):
    """The answer inside the daemon's response envelope (`{"api_version", "value", ...}`)."""
    return payload.get("value", payload) if isinstance(payload, dict) else payload


def fields(record):
    """A claim's fields from an event record (the body holds them under `fields`)."""
    body = record.get("body") or {}
    return body.get("fields", body) if isinstance(body, dict) else {}


class claim:
    """Matches one event: a claim kind, optionally on one subject, with fields equal to the given
    values (or for which a given callable returns true)."""

    def __init__(self, kind, subject=None, **expected):
        self.kind, self.subject, self.expected = kind, subject, expected

    def __call__(self, record):
        if record.get("kind") != self.kind:
            return False
        if self.subject is not None and record.get("subject") != self.subject:
            return False
        values = fields(record)
        for name, want in self.expected.items():
            have = values.get(name)
            if not (want(have) if callable(want) else have == want):
                return False
        return True

    def __repr__(self):
        wanted = ", ".join(f"{name}={getattr(want, '__name__', None) or repr(want)}"
                           if callable(want) else f"{name}={want!r}"
                           for name, want in self.expected.items())
        return f"claim({self.kind!r}, subject={self.subject!r}{', ' if wanted else ''}{wanted})"


class Events:
    def __init__(self, endpoint, alive=None, fail=None):
        self.endpoint = endpoint
        self.alive = alive or (lambda: None)
        self.fail = fail or self._raise

    @staticmethod
    def _raise(message):
        raise EventWaitFailed(message)

    def _get(self, path, timeout):
        connection = _UnixConnection(self.endpoint, timeout)
        try:
            connection.request("GET", path)
            response = connection.getresponse()
            return response.status, json.loads(response.read() or b"null")
        finally:
            connection.close()

    def _request(self, path, timeout, what):
        try:
            return self._get(path, timeout)
        except (OSError, http.client.HTTPException, ValueError) as error:
            # A daemon that exits drops the connection. Name that first, then the transport error.
            self.alive()
            self.fail(f"{what}: the event stream failed: {error!r}")

    def cursor(self):
        status, payload = self._request("/v1/health", 10, "reading the event cursor")
        value = _value(payload)
        index = value.get("store_index") if isinstance(value, dict) else None
        if status != 200 or not isinstance(index, int):
            self.fail(f"the daemon health answer has no store_index: {status} {payload}")
        return index

    def mark(self):
        """A cursor taken now: take it before the action whose consequences are awaited."""
        return Mark(self, self.cursor())


class Mark:
    def __init__(self, events, after):
        self.events, self.after = events, after
        self.seen = deque(maxlen=SEEN)

    def _page(self, subject, remaining, what):
        query = f"after={self.after}&wait=true&limit=200"
        query += f"&timeout_ms={max(10, min(LONG_POLL_MS, int(remaining * 1000)))}"
        if subject is not None:
            query += "&subject=" + quote(subject, safe="")
        status, payload = self.events._request("/v1/events/page?" + query, remaining + 15, what)
        if status == 410:
            self.events.fail(f"{what}: cursor {self.after} is outside the retained event log "
                             f"(a test bug, not a wait): {payload}")
        page = _value(payload)
        if status != 200 or not isinstance(page, dict) or not isinstance(page.get("items"), list):
            # Fail rather than ask again: an unreadable page would be asked for again at once.
            self.events.fail(f"{what}: the event page answered {status}: {payload}")
        return page

    def _next(self, subject, deadline, what):
        """The next page of events after this mark, waiting for one up to the deadline. The cursor
        always moves to `next_after`: a filtered page can be empty while the log moved on."""
        self.events.alive()
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None
        page = self._page(subject, remaining, what)
        if page.get("next_after") is not None:
            self.after = max(self.after, page["next_after"])
        items = page.get("items") or []
        self.seen.extend(items)
        return items

    def _ceiling(self, what, wanted, ceiling):
        recent = [{"store_index": row.get("store_index"), "kind": row.get("kind"),
                   "subject": row.get("subject"), "fields": fields(row)} for row in self.seen]
        self.events.fail(f"{what} (no event matched {wanted} within {ceiling:.0f}s; "
                         f"last {len(recent)} events seen: {json.dumps(recent)})")

    def wait(self, match, ceiling, what, subject=None):
        """The first event after this mark that `match` accepts. The mark moves past it, so the
        next wait continues from there. `subject` narrows the server-side scan; for a `claim`
        with a subject it defaults to that subject."""
        if subject is None:
            subject = getattr(match, "subject", None)
        deadline = time.monotonic() + ceiling
        pending = []
        while True:
            while pending:
                record = pending.pop(0)
                if match(record):
                    # Resume after this record, not after the rest of its page.
                    self.after = record["store_index"]
                    return record
            items = self._next(subject, deadline, what)
            if items is None:
                self._ceiling(what, repr(match), ceiling)
            pending = list(items)

    def wait_state(self, check, ceiling, what, subject=None):
        """For state the log does not carry as one claim: check, and after each new event (on
        `subject`, when given) check again. Returns the first truthy result of `check`. No
        interval: a recheck happens only when the daemon appended something."""
        deadline = time.monotonic() + ceiling
        while True:
            value = check()
            if value:
                return value
            if self._next(subject, deadline, what) is None:
                self._ceiling(what, getattr(check, "__doc__", None) or repr(check), ceiling)
