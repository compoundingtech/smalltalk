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
| `close-item` | Read an informational update or archive a message the person explicitly said to close. |
| `record-go-stop` | Approve or reject an approve-mode human gate for the exact episode and revision the person decided on. |

An unanswered ask cannot be dismissed as an informational update. Attention is derived from
its source: `st attention resolve --for` closes only an informational update. Other attention
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
`st attention resolve`, `st attention approve`, and `st attention reject`. Use the original
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
