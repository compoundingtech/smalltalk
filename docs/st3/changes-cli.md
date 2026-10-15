# Git change reads

> **Status: on hold, do not use yet (Nathan, 2026-10-14).** All changes-in-git work is on hold. Nothing in
> this document is in use, enabled anywhere, or a supported workflow. The feature stays off by default, no
> commit in this repository carries a change id trailer or marker, and no write command, route, pilot or
> Change-Id convention is approved. Do not enable the `[changes]` configuration. The read-only code stays
> in place unused until the work is explicitly resumed.

`st changes list`, `show` and explicit `sync` use configured Git transport and the
node-local graph index. They use no GitHub API. The existing PR workflow remains
available when Git or its designated reader is unavailable.

The feature is off by default. An operator supplies one controlled transport
repository and one reader node per repository/fleet. Requests accept the configured
name only: they cannot supply a checkout, destination or credential command. A
configured destination must match the transport's Git remote before network I/O.
The reader uses its service OS account. Diagnostics show its actual account, reader
node and safe credential-source label; command bodies and tokens are not shown.
This does not establish isolation between seats sharing that account.

```toml
[changes]
enabled = true

[[changes.repositories]]
name = "fixture/garden"
reader_node = "fixture-reader"
transport_directory = "/var/lib/fixture/git/garden.git"
remote = "origin"
remote_url = "https://github.com/fixture/garden.git"
credential_helper = "gh auth git-credential"
credential_source_name = "operator/git-reader"
pilot_started_at = "2026-01-01T00:00:00Z"
```

The example is illustrative, not a service activation. The configured clone and
credentials are operator-owned and outside an untrusted workspace. Authentication
failure retains the old observation and reports its age; it never becomes an empty
successful inventory. A configuration owned by a different node reports the named
owner and the PR fallback instead of starting a second Git reader.

```text
st changes list --repository fixture/garden
st changes show I0123456789abcdef0123456789abcdef01234567 --repository fixture/garden
st changes sync --repository fixture/garden
st changes list --repository fixture/garden --offline
```

A new shared observation performs one patterned `ls-remote`. Concurrent requests
join it; the minimum interval is ten seconds and explicit fetch shares the limit of
six remote reads a minute. `sync` reports in-flight or deferred admission rather
than bypassing the budget. After a completed fetch, a finite importer validates
otherwise unknown retained metadata chains, ancestor first, with a 10,000-record
job limit. It holds no database snapshot or writer during Git operations. A canceled client
does not release the import slot while blocking validation is still running. A larger
history requires a later resumable importer; the PR path stays available.

List pages contain at most 50 changes. Their cursor pins the original observation
for five minutes, is scoped to repository and offline mode, and expires on restart
or bounded eviction. Source/index/evidence freshness remain distinct. An unknown
current object says `sync_needed`; a missing previously indexed change says
`remote_missing`; inconsistent markers say `marker_conflict`. Indexed metadata is
cooperative scheduling data and does not prove a real landing or a live work lease.
Evidence freshness is unknown until separately indexed and verified.

List/show use the existing `read.projections` scope and client envelope. Explicit
sync requires `control.work` because it exercises configured Git credentials and
writes the local derived index. The CLI carries the configured person or the
current `ST_AGENT` over the trusted local Unix socket for sync; list/show keep
their read-only client. Sync accepts no actor selector and refuses an HTTP
endpoint instead of forwarding local identity. An unavailable or non-owner sync is a typed
`unavailable` result naming its reader node and the PR fallback. Imports bind each
current ref's canonical ID to the ID of every loaded or already indexed record;
a mismatch is skipped and reported in a bounded issue list, and cannot set another
change's latest history pointer. A canceled client between fetch completion and
import start can leave fetched objects waiting until another explicit sync.

This importer has a finite 10,000-object job bound, with full unknown lineage
validation before publication. It has no minute-scale wall budget or durable
checkpoint for an unfinished lineage. Very deep cold histories require a separate
resumable importer or the PR path; repeatedly running sync is not promised to
complete a lineage beyond the bound. This read
slice does not add claim/state/source mutation commands, a landing observer,
production pruning, a new claim kind or a binding-contract field. Pilot creation
and stage actions remain off until their separately reviewed authority and bound
checks are available.
