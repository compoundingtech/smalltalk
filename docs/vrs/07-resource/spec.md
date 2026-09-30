# Resource

A Resource is an externally identified thing an agent points at. This document
states the one edge that reaches it, why a second edge existed and was retired,
and what the surface looks like once *resource* names one concept.

Terms are defined in [ontology.md](../ontology.md): [Resource](../ontology.md#resource),
[Resource binding](../ontology.md#resource-binding),
[agent resource directory](../ontology.md#agent-resource-directory),
[working state](../ontology.md#working-state).

Decisions: [0011](../.decisions/0011-the-linked-record-plane-is-retired.md),
[0012](../.decisions/0012-working-state-is-a-declared-carrier.md),
[0013](../.decisions/0013-resource-is-a-mediated-write-surface.md), and
[0014](../.decisions/0014-resource-profiles-are-state-first-read-and-observe-capabilities.md).

## The edge

One agent, one binding, one Resource:

```kdl
resource "work" reason="PR this agent is preparing." \
  uri="github-pr://github.com/example/project/42"
```

The positional name is an agent-local semantic role, unique within one agent.
`uri` is an RFC 3986 absolute URI preserved byte-for-byte, and it is the
Resource's identity. `reason` is required prose saying why this agent carries
it. `inactive-reason`, when present, retains a reference that is no longer
current and explains why.

`selector`, when present, is profile-specific observation configuration. KDL
stores normalized compact JSON in a raw string:

```kdl
resource "work" reason="PR this agent is preparing." \
  uri="github-pr://github.com/example/project/42" \
  selector=#"{"topics":["ci.failure","review.requested"]}"#
```

The canonical renderer chooses the smallest safe raw-string hash fence. JSON
and TOML Agent Spec forms carry the selector as a native JSON value. A selector
is not Resource identity or authority, and an observable profile must validate
it before registration; omission selects the profile default.

The URI is never normalized, and its scheme is the exact lookup key for an
optional, catalog-declared Resource Profile
([`07-resource-profile`](../07-resource-profile/requirements.md), landed in
[#351](https://github.com/compoundingtech/st2/pull/351)). Scheme meaning stays
downstream-owned, st2 ships no built-in profiles, and an unregistered scheme
stays opaque ([R20](../requirements.md)). Possession of a URI grants no
authority, access, or capability
([#61](https://github.com/compoundingtech/st2/issues/61)).

Bindings are desired state. They change through publication under
compare-and-swap, and a binding-only change never stops, replaces, or relaunches
healthy work ([R21](../requirements.md)).

The Rust type is
[`crates/agent-spec/src/spec.rs`](../../../crates/agent-spec/src/spec.rs)
`Resource { name, uri, reason, inactive_reason, selector }`.

## Identity and realization

A binding names a carrier; the carrier's bytes live in the
[agent resource directory](../ontology.md#agent-resource-directory):

| binding | scheme | realizes as |
|---|---|---|
| `notes` | `agent-notes` | agent notes |
| `goal` | `dev.schickling.agent-goal` | `resources/goal.md` |
| `decisions` | `decision-tree` | `resources/context/decisions/` |
| `working-state` | `working-state` | `resources/context/now.md` |

The URI is identity; the path is realization. st2 does not resolve one into the
other, and `resources/` is not a second sense of *resource* — it is where
bindings land. The directory remains canonical for an agent's resource files and
also holds the message planes (`inbox/`, `archive/`, `sent/`) and scratch
material (`tmp/`), which are not carriers.

`working-state` is the sixth self-state carrier
([decision 0012](../.decisions/0012-working-state-is-a-declared-carrier.md)).
Before it, 605 of 655 declarations on one live catalog had a
`resources/context/now.md` and none declared it.

## Surface

`st2 resource` reads and writes the binding plane for one agent:

```text
st2 resource ls [<identity>] [--json]
st2 resource read [<identity>] <name> [--json]
st2 resource refresh [<identity>] <name> [--agent <identity>] [--wait <seconds>] [--json]
st2 resource add <name> --uri <uri> --reason <text> [--inactive-reason <text>] [--selector-json <json>] [--agent <identity>] [--json]
st2 resource remove <name> [--agent <identity>] [--json]
st2 resource rename <old> <new> [--agent <identity>] [--json]
```

```
$ st2 resource ls dev3.dotfiles.fb-batch1.docs.worker
# 5 resources for dev3.dotfiles.fb-batch1.docs.worker
  decisions          decision-tree://dev3/dotfiles.fb-batch1.docs.worker
  dotfiles-checkout  worktree://dev3/…/2026-08-26-fb-batch1-docs
  friction-log       dev.schickling.agent-friction-log://dev3/dotfiles.fb-batch1.docs.worker
  goal               dev.schickling.agent-goal://dev3/dotfiles.fb-batch1.docs.worker
  private-notes      dev.schickling.agent-private-notes://dev3/dotfiles.fb-batch1.docs.worker
```

Rows are ordered by binding name, not declaration order — declaration order has no
meaning. The name column pads to the widest name in the listing. The worktree URI
above is elided at `…`; the command prints it in full.

The name column is aligned to the widest name; the checkout URI is elided in this
document, not by `ls`, which prints every URI verbatim.

The read surfaces project one agent's declared bindings; before them, bindings
were visible only through `st2 agents --json`. `ls` takes an optional leading
identity. `read` and `refresh` treat one positional argument as a binding on the
caller and two as `<identity> <name>`; `refresh --agent <identity> <name>` is
the equivalent explicit-target form and cannot be combined with a leading
identity. Every form defaults through `--as` / `$ST_AGENT`, and every verb
inherits `--catalog`, `--root`, `--as`, and `--host`.

`refresh` asks the resident observable profile runtime for one atomic current
observation. It never rewrites the Resource declaration. `--wait` bounds only
the client wait; timeout or disconnect leaves admitted demand queued. JSON and
durable receipt status values are camelCase; human CLI status text is
kebab-case.

The writes take `--agent <identity>`, perform read-modify-CAS-publish
internally, and emit a stable `--json` receipt, so the caller never renders KDL.
Full-catalog validation, exact-target selection, compare-and-swap, and
fail-closed concurrent-change behavior are preserved, and a binding-only
change does not stop, replace, or relaunch healthy work
([R21](../requirements.md)). This is the fourth instance of the pattern in
[`src/agent_author.rs`](../../../src/agent_author.rs), after streams, desired
state, and presentation
([decision 0013](../.decisions/0013-resource-is-a-mediated-write-surface.md)).
Decisions [0011](../.decisions/0011-the-linked-record-plane-is-retired.md) and
0010 carry "merge and acceptance approval required: upstream maintainers".

Each write is idempotent on its outcome rather than its attempt: `add` upserts
one name, reporting identical bytes as unchanged without republishing them;
`remove` reports an already-absent name as unchanged; and `rename` refuses an
absent name and a collision, since names are unique within one agent. A mutation
refuses an invalid URI, an empty reason, and a Nix-owned declaration, and
preserves the URI byte-for-byte along with the declaration's unrelated bytes.

Bindings remain projected through `st2 agents --json` as opaque descriptors, per
`invariants.md`.

## The retired plane

`st2 resource add|ls|read|remove` previously managed **linked records** — one
markdown file per record with `url`, optional `title`/`tags`/`relation`, and an
optional body, under `<agent-dir>/resources/links/`. An agent wrote them itself,
without publication or CAS, to record what it produced.

That plane is retired
([decision 0011](../.decisions/0011-the-linked-record-plane-is-retired.md)).
Recording produced artifacts is `axe work update --artifact <path> --pty <name>`.

`templates/bus.st2.md` advertised `st2 resource add|ls|read|remove` to every
agent on the bus, which is what produced the adoption measured below. It now
carries the binding verbs instead.

### Why it existed and why it went

Measured on one live catalog of 655 declarations
([experiment](.experiments/2026-08-27-resource-read-surfaces.md)):

- 1161 bindings across 647 declarations; 241 linked records across 82.
- 889 distinct binding URIs and 233 distinct linked-record URLs, **zero
  coinciding** — exactly, and normalized across all five schemes both planes
  used. 81 of the 82 linked-record-carrying declarations also carried bindings,
  so the planes coexisted constantly and never named one thing.
- The only reader of a linked record in the source tree was its own `ls`/`read`
  verb. No projection, roster, or doctor consumed them.
- Adoption was 12.5% of declarations and decaying: 45 records on 2026-08-08,
  five on 2026-08-26, none after.

The disjointness was semantic. Bindings carried what an agent *is for* — 728
self-state carriers and 378 work inputs. Linked records carried what it *made* —
196 of 241 were `output`, `produces`, `evidence`, or `verified`. Where a
declared worktree and a linked output shared a path the relation was
containment, the product sitting inside the input, not identity.

So a shared descriptor type would have deduplicated nothing and joined nothing.
[#122](https://github.com/compoundingtech/st2/issues/122) proposed exactly that,
around a `_tag` field removed in #307.

### What the split actually cost

The remaining 19% of linked records name the mechanism: `supervises`,
`reference`, `current-work` (which duplicates the `work` binding),
`depends-on-slice-*`, `blocked-design`. Those are dependency and reference edges
filed in the products plane.

An agent could not cheaply record a dependency. A binding needed publisher
authority and whole-declaration republication under compare-and-swap. So it
wrote a linked record, because that was the plane it was permitted to write.

**The drift was caused by write-cost asymmetry, not by two relations.** Retiring
the plane removes the escape hatch; the mediated write verbs
([decision 0013](../.decisions/0013-resource-is-a-mediated-write-surface.md))
remove the pressure that produced it.

### Reported friction

Two declarations, from
[dotfiles#2071](https://github.com/schickling/dotfiles/pull/2071): one with five
bindings and no linked records reported `# 0 resources`; one with two bindings
and fourteen linked records reported fourteen rows containing neither binding.
Both answers were correct and neither surface said which question it answered.

The read surfaces were also inverted against their authority. Bindings were
governed by R20 and R21 and pinned by an invariant, and had no human surface at
all. Linked records were governed by nothing and owned the short, obvious verb.
An agent looking for its bindings found the linked-record verb first, because it
was the only one whose name matched what it was looking for.

The boundary had been correct in writing since 2026-07-30:
[#61](https://github.com/compoundingtech/st2/issues/61)'s resolution states that
`st2 resource` manages linked-reference records and does not mutate a binding.
That sentence never reached the command's own words. With one plane, it no
longer needs to.

### Residue

The 241 existing records stay in place as orphaned files. Most belong to retired
declarations whose worktrees are gone, and `axe work` writes to a gitignored
per-worktree path that no longer exists for them. The ontology keeps a retired
entry so a reader who meets one can identify it.

## Boundaries

Nothing here claims mutation authority over the thing a URI names, and nothing
makes possessing a URI mean something. A mediated write changes a declaration;
it does not touch the referent.

Resolution is deliberately out of scope for this document, not absent from st2:
[#351](https://github.com/compoundingtech/st2/pull/351) owns the scheme-keyed
Resource Profile registry under
[`07-resource-profile`](../07-resource-profile/requirements.md). The two are
orthogonal — a binding names a Resource whether or not any profile resolves its
scheme, and `st2 resource` neither reads nor requires one.
