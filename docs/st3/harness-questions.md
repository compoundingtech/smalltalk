# Harness questions

A seat's harness can ask its person a question natively: a multiple-choice prompt the harness
shows in its own interface. st records that such a question exists, how it is shaped and how it
ended, so other clients can tell the question is waiting. It records **ids and positions only,
never text**. The question text, option labels, free-text answers and notes stay in the harness's
own timeline; a client that shows the question joins its text from there.

## Identity

A question is the subject `harness-question/ID`. `ID` is the first 16 bytes, in hex, of a SHA-256
over the agent subject, harness session id and canonical native call id, each length-prefixed
(`st3_schema::harness_questions::subject`). The canonical native id is `sha256:` plus the
64 lowercase hex digits of SHA-256 over the raw native call id. The store hashes every raw
native id before deriving the subject or storing the observation; paths, command strings and
other native text are never stored verbatim as ids. The same native call in the same session
of the same seat is always the same subject.

The claim subject must match its `(agent, session, native_id)` parts. Cross-field subject
validation is not implemented by this schema-only PR; it lands with the first production
callers. Those callers must use the canonical hash before deriving the subject.

## Claims

Both kinds are system-only: only the daemon writes them, on behalf of the harness adapter.

| Kind | Fields | Meaning |
|---|---|---|
| `harness-question.observed` | `agent`, `session`, `native_id`, `runtime_incarnation`, `questions` | the call as the harness shows it. Each question is `{id, options, multi, required}`: its id, its number of options, whether it takes several and whether it needs an answer |
| `harness-question.resolved` | `status`, `revision`, `answers`, `receipt` (optional) | how the call ended: `answered`, `answered_elsewhere`, `expired` or `cancelled`; the revision it settled; per question id, `{positions, timed_out}`; and the `harness.answer` request that settled it, as two digests and its action id |

A call has 1 to 16 questions, a question at most 64 options. Question ids and answer-map keys
must be canonical zero-based positional ids `q0` through `q15`, or `sha256:` followed by the
64 lowercase hex digits of a hash of the native question id. They must never be native
question text, paths or commands. Session and incarnation ids remain bounded opaque ids.
There are no stored option ids: unique answer positions directly index the native option
order, so position-to-option mapping never depends on a question or option id.
`timed_out` is the harness's own per-question flag.

**Revision.** A question's content revision is the ID of its newest observation. The harness
observing the same content again writes nothing, so the revision holds; new content (another
option count, another runtime incarnation) is a new observation and a new revision. Settling the
question writes the resolution and leaves the revision unchanged. A resolution names the revision
it settled and is written only while that is still the newest observation.
An observation is fenced on the revision inspected before appending (or on absence for a new
question), so a concurrent resolution in the same store cannot be followed by new content.

The question reader (`Store::harness_question`) is the only reader contract for these subjects.
The generic subject views (claims, actual state) show the claims while the store holds them.

**Once, locally.** The write-once resolution check is local, not a distributed compare-and-set.
A second local resolution returns the first; a resolved question takes no new observation.
Two disconnected writers can both return success before seeing each other's resolution.
Replay converges to the first resolution in canonical claim order. The losing writer has
already reported local success; after replication its reads show the winner's status,
positions and receipt, not its own. A resolution retry returns that winning record with
`written = false`; its losing receipt no longer satisfies `settled_by`. This is not proof
that a native answer was delivered only once. Cross-writer native settlement is work for the
production caller PR, not implemented here.

## Retention

A pending question is kept whole until it ends. The owner's compact record — question id,
status, answer positions and receipt — is retained indefinitely and never hidden by age,
therefore at least as long as the native timeline entry, even if that entry later expires.
The newest observation (identity and option shape) and the resolution stay in the claim log
and in `Store::harness_question` forever.

Only redundant observation history may expire: a checkpoint whose cut is at least one day
after resolution (`RESOLVED_RETENTION_MS`) keeps the newest observation and drops older
observations, subject to the ordinary checkpoint guards. No text or bulky harness timeline
data is stored in these claims. Independently derived bulky data may expire, but never the
compact owner record.

The rule `all-while-pending,newest-once-resolved-1d-before-cut` uses rules version 18,
following #1473's allocated version 17 and main's review-reader version 16. Whichever PR
lands second must renumber in a plain follow-up commit. The proof compares the same compact
question before and after compaction. Resolved questions never become pending again: re-observation of the
same native call is refused even after history expiry or checkpoint trimming, including when
the resolution status is `expired`. Its original resolution and receipt remain authoritative.

A question stays pending until the harness writes its end. The harness adapter writes `expired`
or `cancelled` when the harness gives up on or withdraws a question; st does not end a pending
question by itself.
