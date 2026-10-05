# Typed custom subjects

A kind registration adds field validation and a bounded, data-only projection to the existing
`custom/NAMESPACE/NAME` subjects and `custom.NAMESPACE.NAME` claims. The graph stores the
manifest, facts and answers. The daemon interprets a descriptor; it runs no extension code.

## Register and answer a question

The complete example manifest is [custom-review.json](../../examples/st3/custom-review.json).
All identities below are invented. Run the commands against your own daemon:

```sh
st schema register examples/st3/custom-review.json --as agent/garden/seed
st claim custom/garden/review/v1/review-001 custom.garden.review.v1.requested \
  --actor agent/garden/seed \
  --field 'title=Retain the seed history?' \
  --field 'detail=Choose Keep or Discard.' --field recipient=person/lichen
st subject show custom/garden/review/v1/review-001
st attention ls --as person/lichen
```

The claim pins its registration hash automatically. One card appears for Lichen. stui displays
its declared fields and source ID. Open the reply box and enter `keep`, or enter a JSON object
such as `{"selection":"keep","text":"Retain the provenance."}`. The generic client action
is `custom.reply`; its parameters come from the card's `action_parameters` and `custom_form`.
It requires `control.attention` and the addressed person's authority.

For a CLI reply, copy the `registration`, `revision` and attention `episode` from `subject show`,
write the reply fields to a JSON file, and run:

```sh
st subject reply custom/garden/review/v1/review-001 \
  --registration HASH --revision REVISION --episode EPISODE \
  --fields-file reply.json --idempotency-key garden-review-answer-001 --as person/lichen
```

The answer records Lichen's actor and the immutable request claim ID, removes the card, and
survives restart and replication. Stale card parameters write nothing. An exact retry returns
its previous claim; changing the request under the same key is rejected. Concurrent answers on
isolated members remain competing immutable facts, visible as `reply_conflicts` and in history.

## Manifest contract

`st schema registrations` lists selected registrations; `st schema registration KIND@HASH`
reads an exact registration. Registration is a trusted local configuration operation and uses
existing rules. Paired devices cannot register kinds. Bound agent sockets cannot name another
actor. Human replies still require the fixed recipient; free mode does not let an agent answer
as a person.

A manifest declares `kind`, `namespace`, positive `version`, `language: custom-projection.v1`,
exclusive `subject_prefix`, `creation_kind`, `claims`, `slots`, `fields` and optional `attention`.
The registry subject for `garden.review` version 1 is `custom/st3-kinds/garden/review/v1`.
`custom.st3-kinds.registered` records an exact immutable manifest document reference. Ordinary
raw writes cannot publish registry records. The internal `client` and `st3-kinds` namespaces
are reserved. Unregistered custom subjects retain their existing open-field behavior.

A kind/version is immutable. Identical registration is idempotent; changes require another
version and exclusive prefix. Registration cannot capture a prefix with existing untyped
facts. Offline competing registrations or overlapping prefixes disable the affected views.
Unsupported or unavailable manifests stay in raw history, with pinned sources pending until
supported manifest dependencies arrive.

Each claim schema declares an authority (`creator`, `owner`, or `recipient`) and fields using
existing schema types. Creation fixes the owner actor and addressed person. Only creation
claims have creator authority. Owner claims require that original actor; recipient claims
require the original person. Raw `st claim` writes receive the same validation as generic
replies. Fields support required/optional values, scalar types, enum strings, arrays with
`min_items`/`max_items`, subject reference families, immutable documents (`document: true`)
and immutable claim IDs (`claim: true`). `additional_fields` defaults to false. Document/claim
reference flags require string fields. `_registration` and `_basis` are reserved metadata.

Slots select `first` or `last` in canonical order for an exact declared claim kind. Expressions
copy a field (`op: field`, `slot`, `field`), an actor or claim ID (`actor`/`claim-id`, `slot`), or a
scalar constant (`constant`, `value`). Predicates are `exists`, `eq`, `all`, `any` and `not`.
Attention declares `when`, `recipient_field`, `title`, `detail`, `episode`, and a `reply` with a
claim kind, user-input fields, and expression bindings. The recipient is a required person
reference in the creation schema. Reply kinds must require recipient authority.

Limits include a 64 KiB manifest and claim field object, 32 slots/claim kinds, 64 output fields,
128 predicate nodes, depth 16, a 64 KiB projected row, and 256 basis dependencies. Invalid
facts or outputs disable only their source. Each source evaluates inside a savepoint; genuine
storage errors remain storage errors. No custom SQL, callbacks, loops, network requests,
retention rules, native actions, or contributed UI are accepted.

## Reads, replication and derived state

`GET /v1/client/custom-subjects` pages the source index, with optional `kind`, `version`, `cursor` and
`limit`. `GET /v1/client/custom-subjects/{encoded-id}` reads a source. Both require
`read.projections`; the source is an extensible `custom-subject` resource, preserving old
client unknown-resource decoding. It includes the pinned registration, source revision,
selected fields, provenance and `state`: `ready`, `stale`, `pending-dependencies`, `conflict`,
or `invalid`. Custom attention adds optional metadata to the existing attention resource.
Updated clients send `x-st3-features: custom-subjects.v1` to receive namespaced attention and
`custom.reply`. Without that opt-in, attention/Now pages and collection streams use the existing
`agent-request` kind, no new action enum value, and an exact CLI reply hint. Older closed-enum
clients keep reading their entire queue. Optional custom metadata remains available to renderers.

Manifest documents, registry claims and typed facts use the existing signed envelope and blob
replication. The three shared custom tables are rebuildable caches and participate in projection
digests. Rows use canonical claim identities, never local arrival indexes. Checkpoints keep
custom history and its referenced blobs. Client reads page indexes and never fold custom history.

An external domain tool can append a derived claim with `_basis`, an array of
`{"subject":"custom/...","kinds":["custom.domain.input"],"revision":"HASH"}`. Obtain the exact
input-set token with `st subject basis SUBJECT --kind custom.domain.input` (or the equivalent
`/v1/custom/basis` read). Another matching input claim immediately makes the derived view stale
and removes its card. The tool recomputes and appends a fresh derived claim. Filters exclude
the output kind, dependency cycles are invalid, and bounds are enforced.

For a decision tree, register request, answer, assumption, promotion and derived-status kinds.
Keep raw human answers and supersession references immutable. The external tool owns guards,
option grounding, handle allocation and fork detection. Only fresh `pending` derived states
should satisfy the attention predicate. A revived question uses a fresh derived claim ID as
its episode. Array selections can allow zero or multiple choices, and a text field preserves a
freeform reframe. Custom claims do not block native work or confer native approval; a tool that
needs native blocking separately uses authorized work asks. This slice does not replace the
existing decision-tree bridge.
