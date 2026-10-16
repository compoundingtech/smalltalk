# Recording a person's instructions

An agent can record an answer or instruction its person already gave while keeping its own
actor identity. The person selects the allowed actions first. This does not allow the agent
to decide for the person, perform external actions as them, or delete anything.

From the person's terminal, replace the whole allowed list:

```sh
st work delegation --for person/avery --as person/avery \
  --action answer-ask --action close-item --action record-go-stop \
  --evidence POLICY_DECISION_CLAIM_ID
```

The response's `id` is the policy reference. Read its history with `st subject show person/avery`.
Omitting every `--action` records an empty list and revokes delegation. Only the person can
establish or replace their policy; an agent cannot name that person as its actor. There is no
default policy, and old policy references cannot authorize new actions after replacement.

| Action | Permitted operation |
| --- | --- |
| `answer-ask` | Complete a person-assigned ask using their prior answer, keeping its named answer ID and text. |
| `close-item` | Read an informational update, archive a message, or dismiss an auto-denied harness call the person explicitly said to close. It cannot allow a call or its kind. |
| `record-go-stop` | Approve or reject an approve-mode human gate for the exact episode and revision the person decided on. |

An unanswered ask cannot be dismissed as an informational update. Attention is derived from
its source: `st alerts resolve --for ... --outcome resolved` reads an informational update. With
`--outcome dismissed`, the same proof flags can close an exact auto-denial episode. Other attention
requires answering or remedying its source. Authored person steps, feedback changes, launch
approvals, mission revision approvals, and other operations are outside this initial list.
Existing free-mode actions remain recorded as the agent itself.

For an ask, the agent uses the original `work.person-asked` claim ID as the episode:

```sh
st work done step-run/example/ask --as agent/example/helper \
  --for person/avery --policy POLICY_CLAIM_ID \
  --instruction message/date-answer --quote 'Friday' \
  --episode ASK_CLAIM_ID --summary Friday
```

The same proof flags work on `st conversations archive` (one message at a time),
`st alerts resolve`, `st alerts approve`, and `st alerts reject`. Use the original
`message.sent` claim ID for an archive, the update's `work.person-asked` ID for a closure,
or the exact `gate.requested` ID for a review. `st work done --answer ID --text TEXT` retains
structured answer validation. A delegated cancellation is refused.

The daemon's store checks the current policy, operation, assigned person, episode and revision,
the instruction's original recorded sender, and a nonempty verbatim quote. The source must be
the person's own `message.sent` claim; an agent-authored summary or a declaration pretending
to be a person message does not qualify. Immutable document-backed message bodies are supported.
These checks run on generic claim writes too, so supplied audit fields cannot bypass them.

The agent still has to understand whether the quoted words authorize that particular target
and answer. A substring check does not interpret intent or turn a quotation of somebody else
into the person's instruction. Ambiguity and decisions they have not made remain with them.

Every completion records the agent actor, `acted_for`, its full `delegation` proof, and the
instruction and policy in its evidence. Person answers expose both `respondent` and optional
`acted_for`; requester notifications also identify the agent that recorded the answer.
Replicated claims and history retain the proof. Identical completion retries do not repeat
the action; changing the answer or proof under the same key fails.

The CLI uses `/v1/work/delegation`, `/v1/work/done`, `/v1/reviews/...`, and the message lifecycle
endpoint. These accept an optional `delegation` object with `person`, `policy`, `message`,
`quote`, and `episode`. Paired client-v0 actions retain their existing session authority
rules; the `person_answers` projection includes delegated answers for those clients to read.

For an auto-denial, name the alert's `attention/...` ID and the original
`harness.diagnostic` claim as `--episode`. The instruction must have been sent by the
represented person to the agent recording it. The action leaves the call denied and records
the agent, represented person, current policy, instruction and exact episode. Its source
answer and the notification to the seat commit together. A live permission prompt, approving
once, and allowing a kind are outside this grant; they are refused rather than inferred from
`close-item`. `prompt.respond` carries the same optional proof in client-v0; its actor is the
authenticated session authority, never a parameter supplied by the client.

The instructed agent need not be the seat that raised the denial. A person may instruct a
different agent to dismiss a denial on any seat currently owned by that person; the source
seat receives the notice. The writer rechecks the seat's person, the current policy and the
exact episode. This extends the meaning of an existing `close-item` policy, even though it
does not write a new policy claim; it requires the recorded authority-contract acceptance.

Both new actions require a compatible rollout so the owner can read their retained history.
Delegated denial dismissal is unavailable on a mixed-build fleet. Before anyone records the
new diagnostic or uses sender withdrawal, Operations must verify that **every admitted
replication member**, every other eligible receiver and the recipient's client-serving daemon
have a build with this claim schema, source rule and history endpoints. The known four
deployment hosts do not define that set. An offline or unverified admitted member keeps the
operation unavailable; a removed member must upgrade before re-admission or replay/rejoin.
Do not resume an older receiver against this history before it upgrades. This is a rollout
order, not an automatic capability gate or permission to change membership.
Before use, the Operations rollout receipt must name each actual member and eligible
receiver, its installed full source/build, and whether its claim schema, source rule,
history fold/read and client-serving daemon are available. Include offline and rejoining
members explicitly. This source change supplies no qualified host inventory; missing
receipts mean availability is unknown, not permission to rely on the widened policy.

An older registry does not ignore the extra audit fields: it holds the authenticated denial
answer as `unknown-claim-field`, outside admitted/projected claims, for later revalidation.
The separate ordinary seat notice has known fields and can be admitted meanwhile. The older
member can therefore show a still-open denial alongside that notice. After upgrade, backlog
validation admits the original answer bytes; it does not send the notice again. The private
old-registry signed-exchange control pins these expected receive outcomes; its execution is
required before qualification. An older client-v0 server ignores the optional proof parameter
and then refuses the agent through its ordinary person-only answer check, possibly with a
misleading person-only answer error. Use a server containing this change before sending proof.
`dismiss` already worked on the person's server path. The public answer remains main's
bounded string, including plan, question and screen answers; the optional proof narrows
an agent's answer to `dismiss`.
On the wire, ordinary person requests still omit the proof. Swift's initializer defaults it
to nil and TypeScript's property is optional. Rust callers using a struct literal must add
`delegation: None` or use the existing default; this is a source adjustment for those callers.

An agent can withdraw an informational update it sent without acting for the recipient:
`st alerts withdraw STEP_RUN --as AGENT --reason REASON --idempotency-key KEY`. To use the
attention card ID instead, also supply the update's original `work.person-asked` claim as
`--episode`. This cancels that update source, preserves its history and leaves unrelated
claimed work alone. It cannot withdraw another sender's update or use this route to cancel
a question; an explicit ask keeps `st work cancel-ask` semantics.
Source-kind validation currently precedes requester authorization: an ask receives
`invalid-update-withdrawal` even from a stranger, while a stranger's actual update
receives `forbidden`. The error can distinguish those source kinds; neither refusal
authorizes an action or reveals the update's text.

Withdrawal removes the update from open Home, not from its recipient's client history.
`GET /v1/client/attention?history=true` (also `/v1/client/alerts`) lists resolved source
records for sender withdrawals and delegated denial dismissals. With no work filter,
`GET /v1/client/now?history=true` serves the same history. The original update title/text
and request remain readable, marked withdrawn by the sender with its reason. A dismissed
denial retains its original reason, the agent and represented person, instruction reference
and quote, policy and episode. These rows offer no action and are not open alerts.

This changes the meaning of `history=true`: attention and unfiltered Now previously
ignored that flag and returned the open list; they now return only closed source items.
Older daemons still return the open list, which is not closed-history evidence. Attention
history rejects `actor`, `owner_run`, `status`, `owner`, `native_only=true`, and a `state`
other than `resolved` with 422 `validation-failed`. Now history without `actor` or `owner_run` rejects
`status`, `owner`, `native_only=true`, and a non-resolved state with 422 `validation-failed`. With `actor`
or `owner_run`, Now retains its existing work-filtered open/combined list and validation;
that request is not routed to closed history. No in-repository caller currently sends
these history requests. The history read path is a separate client behavior alongside
dismissal and withdrawal, not a claim that open Home now includes closed cards.

Each closed resource has an `attention/closed/CLAIM_ID` ID naming its immutable resolution;
select that ID through the ordinary attention/detail endpoint. The old hashed open-card ID
is an open projection identity, not this closed source identity. Recipient filtering uses
the original update recipient or the recorded represented person, so a later seat owner
cannot take over the previous person's history. A person's authenticated session cannot
select another person's item or change the cursor's recipient.

A person session is forced to that person's recorded-owner filter. Another person's
closed detail returns 404. As with the existing attention list, an agent session
that supplies no `person` filter can read all owners' retained resolutions; this change does
not invent per-agent privacy or impersonate a person.
If a readable resolution still establishes the owner but its original is oversized,
that owner's exact detail returns 503 while another person's read returns 404. The
status distinguishes an unavailable own item from an absent or unauthorized item;
it does not expose the source text. An unreadable row that cannot establish ownership
also returns 404 to a person session.

History pages scan at most 32 candidates plus one lookahead from the two indexed source
kinds before recipient/type filtering. Each kind producer takes at most 33 indexed entries;
only their at-most-66-entry union is sorted. An empty page can still carry `next_cursor`:
clients must follow that cursor, rather than treating an empty `items` array as the end.
For N relevant-kind candidates in the captured cut, the worst case at limit 32 is
ceil(N/32) pages even if only one row belongs to the person. Ten thousand such candidates
therefore require 313 pages; there is no constant-page or guaranteed recent-item density.
The per-request budget remains 32 processed candidates, 1 MiB per body, 2 MiB cumulative
body decode and half the existing response byte budget for items. The existing request
read deadlines still apply. The pagination time budget is the existing five-minute cursor
lifetime, not an extended read deadline. Ten-thousand-row traversal and the exact index/
sort plan are authored hosted controls, not measured cost or time evidence. If the cursor
expires before traversal finishes, clients must report incomplete history and refresh;
they must not claim an empty or complete history or run an unbounded automatic scan.

`page.omitted_items` is an **approximate** diagnostic count, present only on this history
path, including zero. It can undercount rows that decode but cannot bind to a valid
original in the resource mapper, and overcount damaged rows whose owner or relevance
cannot be established. Zero is not proof of complete owner history, and the count is
not an exact number of missing items for that person. Known unrelated
diagnostics are identified by a SQL relevance flag and skipped without Rust decoding,
decode-budget charge or omission count. SQL still validates and classifies bounded JSON
per candidate and can parse or traverse it more than once; that work is not charged to
the Rust decode-byte budget. The indexed `claims_kind_index` plan and limits above are
unchanged, and do not certify negligible SQL cost. A malformed or over-width diagnostic body cannot
be classified safely; it contributes only an anonymous, conservative omission rather than
requiring unbounded parsing to prove it is noise. The count includes scanned candidates that could not be safely decoded, had a detected missing linked source at the captured cut,
or exceeded the per-page decode/item byte budget. The cursor advances past those indices.
Clients must retain an incomplete-history warning when this count is nonzero, even on the
last page, and refresh after source replication/repair. Known foreign-owner rows are
filtered before decoding or source checks and do not increment that person's count. If a
row is too damaged to establish its owner, only an anonymous omission count is exposed,
never its identity/text. This count is not a claim that every omitted candidate is an
attention item. An unreadable exact detail is unavailable (503) only for an authorized or
unfiltered read; another person's detail stays 404. Existing page paths omit this optional
field. Older Rust and Swift decoders ignore this optional extra field. The TypeScript schema's
`decodeUnknownSync` and `decodeUnknownEffect` helpers default to `mode="tolerant"` with
`onExcessProperty: "ignore"`; an explicitly requested `mode="strict"` instead rejects
unknown fields. The generated fetch client returns JSON without an automatic strict schema
decode. These are source-path statements, not executed old-build compatibility results.
Older consumers cannot present the new completeness guarantee; client consumers must honor it before claiming complete history. Rust struct literals
constructing `PageInfo` also need the new optional `omitted_items` field.

The signed cursor fixes the original cut, recipient and filters for five minutes and
expires on daemon restart because its HMAC key is process-local; refresh to see newer
resolutions. Detail is a direct claim lookup. Reads never write, backfill or rebuild.
Existing durable update/message and cited-evidence retention keeps the update and the
denial's answer/instruction/episode chain; this adds no retention expiry or deletion.
