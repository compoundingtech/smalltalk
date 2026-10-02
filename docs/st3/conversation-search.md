# Conversation search

```sh
st conversations search "release date"
st conversations search "release date" --agent agent/example/worker --since 2026-10-01T00:00:00Z
st conversations search "release date" --cursor CURSOR --json
```

The client must identify a concrete person (`--as person/ada` or the configured person).
Search requires `read.projections`. A paired device searches under its delegated person,
never a person supplied in a query. The mailbox source contains that person's sent and
received Small Talk messages, including archived messages. Transcript visibility follows
the same session inventory and normalized timeline reads as `conversations sessions --all`
and `conversations timeline`; free mode currently makes those agent transcripts visible
to every person with the projection scope. Remote agent timelines use the existing
authenticated owner-host relay. Search does not read raw terminal screens.

The query is a literal Unicode word phrase, case insensitive. Punctuation separates words;
FTS operators are ordinary query text. It has at most 512 UTF-8 bytes and must contain a word.
`--since` takes an RFC3339 timestamp and includes entries at that instant. Results sort by
timestamp descending, then conversation ID and entry ID descending for stable ties.
Each hit gives `conversation_id`, `entry_id`, optional `agent_id`, `timestamp`, `entry_type`,
and an excerpt of at most 512 characters. Message hits use the canonical message ID for both
targets; transcript hits preserve their normalized session and timeline entry IDs. `--agent`
selects that agent's transcript and messages exchanged with the person.

`GET /v1/client/conversations/search` is the typed read `conversation.search`. The generated
clients expose Rust `Client::conversation_search`, TypeScript `St3Client.conversationSearch`,
and Swift `St3Client.conversationSearch`. They return the same `ConversationSearch` response,
including `page`, `indexed_at`, `host_id`, `refreshing`, and `incomplete_sources`. Pages have
1–200 hits (50 by default). Cursors bind the reader, query, filters, page size, and index
revision. A changed revision or evicted index returns `cursor-gap`; restart the search.

The index is a disposable, daemon-local SQLite FTS5 database in memory. It is derived from
durable message text and st's existing normalized timeline entries, with no new graph facts,
replication vocabulary, or client-owned store. A first search starts its index in the
background and waits at most two seconds. If a large inventory is still being read, the
response is retryable `remote-unavailable`; that work continues. Later first-page searches
trigger a refresh at most once per 30 seconds and return the previous dated index with
`refreshing: true` while it runs. Repeat the query once the refresh finishes to see new text.
There is no idle search timer and no extra work on the graph writer's critical path.

Mailbox change detection reads the existing from/to expression indexes; unrelated fleet
writes do not rebuild message text. A changed mailbox is rebuilt newest first. Managed
timeline change detection includes incarnation, observation identity and count (including
prefix deletion), plus native transcript size and modification time. Unchanged local
timelines are reused; changed timelines are normalized and paged once per refresh. Remote
timelines are rechecked on a demand refresh because their local replicated observation may
lag the file. Replacements and finalizations replace old indexed text. A missing or
unreadable source drops its old searchable entries and reports the source.

A daemon keeps at most four reader indexes and runs at most two refresh workers. Each index
contains at most 50,000 entries and 16 MiB of text, plus SQLite postings and IDs. Refresh
temporarily holds changed text and one normalized source's bounded timeline; it reads no
more than 50,001 mailbox records. Query work runs on a blocking worker and returns at most
201 rows to determine pagination, with at most 200 sent to the client. The index consumes
no additional disk writes. Cold cost is one inventory read, message projection, and a
normalization pass over available source windows; warm queries read FTS postings, and
refresh cost depends on changed sources and reachable remote timelines.

Search covers the history that st's normalized readers retain and expose. Their native
transcript and inventory limits still apply; it is not an archive of discarded transcript
prefixes. Truncation, redaction, malformed history, offline hosts, and index budget limits
are reported in `incomplete_sources`, even when a query has zero matches. At most 128 source
warnings are returned, with a count if more were omitted. A refresh failure also reports
that the dated index could not be updated. Clients should display this metadata when
interpreting a missing match.

For an embedded index independent of the daemon, `st3::conversation_search::SearchIndex`
accepts caller-owned, already-authorized `SearchEntry` sources with `replace` and `remove`,
then produces `SearchHit` values with `search`. The caller owns discovery, access control,
refresh scheduling, and corpus bounds; the index never reads harness files itself.
