# Resource subscriptions

Status: current design.

## Outcome

An agent or a person can request a message or mission when selected external facts change.

The request is durable graph state. A supervised observer checks the external resource without using an agent turn.

The providers observe one GitHub pull request, one GitHub repository, one GitHub branch ref, or one
local file.

The GitHub provider supports `head`, `state`, `review`, and `checks`.

One `github.repository` observer covers one repository. Its fields name the data types it emits:
`pull_requests`, `issues`, `comments`, `reactions`, and `mentions`. Each subscription on it adds the
types it selects, and the observer emits nothing else. One poll serves every type, however many
items the repository has, so no pull request or issue needs an observer of its own.

Each pull request and issue is its own resource, `RESOURCE/pull-request/NUMBER` (kind
`vcs.pull-request`) or `RESOURCE/issue/NUMBER` (kind `vcs.issue`). Each item resource holds the item's
latest state. The repository resource keeps only its own facts, such as `repository_id`.

| Data type | Facts on each item |
|---|---|
| `pull_requests` | `number`, `url`, `title`, `author`, `created_at`, `state`, `merged` once closed, `draft`, `head_sha`, `branch`, `base_branch`, `checks_state`, `checks` (each `name`, `status`, `conclusion`), `required_checks` (`state`, `source`, `checks`, `failed`), `review_decision`, `reviews` (each reviewer's latest `state` and `commit`), `merge_queue` (`state`, `position`) while queued, and `opened_by`/`opened_by_run` |
| `issues` | `number`, `url`, `title`, `author`, `created_at`, `state`, `state_reason` |
| `comments` | `comments` (the count), `last_comment` (`id`, `author`, `url`, `created_at`, `updated_at`, `body_digest`), and `recent_comments`: the newest 20 conversation comments and submitted reviews, oldest first, each `kind` (`comment` or `review`), `id`, `author`, `at`, and a review's `state` |
| `reactions` | `reactions` (each reaction's count), and `last_comment.reactions` |
| `mentions` | `mentions`: the newest mention of each GitHub login in the item's body or comments (`login`, `by`, `url`, `at`), up to 50 |

A comment body never enters the graph. `body_digest` tells one body from another, and `url` leads to
the text. A subscription that selects `comments`, `reactions`, or `mentions` hears a change of that
type only; a check, a review's state, or a new head is a `pull_requests` change, and a new entry in
`recent_comments`, a comment or a review, is a `comments` change.

`recent_comments` keeps every comment and review once, whichever read saw it, so two comments
between polls are two entries. An edit keeps its entry. A pull request's reviews join it only on an
observer that emits `comments`.

`required_checks` says how the checks a pull request's base branch requires stand on its head.
`checks` names the checks that count. With `source` `rules` they are those the base branch's
rulesets and classic protection require. A check matches by name, and, when the rule names an
app, only a check run from that app counts; a commit status names no app, so it counts only for a
rule that names none. With `source` `all` the base requires none, or GitHub would not show its rules, and every check
on the head counts. `state` is `pass` when every counted check finished as success, neutral or
skipped, `fail` when one finished any other way (named in `failed`), `pending` otherwise, including
while a required check has not appeared, and `none` when nothing counts. Optional checks never
decide it.

The `github.ref` provider supports `head` and `ancestors`. `head` is the selected branch's commit SHA.
`ancestors` contains the full `refs/heads/NAME` name of every other repository branch whose head is
reachable from the selected branch.

A live subscription to a `github.ref` observer now checks at least every thirty seconds, using the existing shared ETag cache and conditional requests. An explicit faster `every` interval is retained. Unchanged HTTP 304 responses reuse the complete normalized facts and do not create another resource observation or delivery. Without a subscription the observer retains its ordinary five-minute default or its authored interval. Rate-limit retry deadlines remain authoritative.

For mission deliveries from a `vcs.ref`, only the newest observed head remains queued. Older unstarted deliveries are cancelled before capacity retries and stay cancelled across daemon restart. Run creation rechecks that head in its writer transaction. A running mission retains its original pinned resource claim and its exact-commit `ci-passed` gate. Keep the applier mission at `concurrent-runs max=1` and on its existing host; no push receiver or additional applier is introduced.

Every item the observer sees becomes its resource, including a draft pull request and every open
item at the baseline. Only a change records an observation: an unchanged item records nothing, and
an observation that read only some facts, such as a new comment, keeps the others.

Agents open pull requests with a shared GitHub identity, so the author login cannot say which agent
opened one. When a pull request appears or moves to a new head, the observing host looks for the
agent on that host whose workspace has the pull request's branch checked out. When exactly one
agent has it, the listing records that agent as `opened_by` and its mission run as `opened_by_run`.
A pull request keeps an opener once named, so a reviewer or fixer that later checks out the branch
does not take it over. Like an issue's, each attribution fact stays across every later write to
the pull request resource: a publisher's partial snapshot such as `{state: merged}`, a
`github.pull-request` observation that never reads it, and a later claim that names another opener
(Nathan, 2026-10-03, #778 rule 5). When no agent is named, a mission run that published
`resource/mission-run/RUN/pull-request` for the pull request becomes its `opened_by_run`. A review
mission routes its findings to that agent or run.

An issue resource accepts the same optional `opened_by` and `opened_by_run` facts from its
publisher, such as the seat that opened it. st does not infer an issue opener. Each attribution
fact, once named, stays across later publisher writes and issue or repository observations.

The local file provider supports `status`, `path`, `content_hash`, `size`, `mode`, and `reason`. It never returns file content.

## Authored watch operation

A planner or authorized producing agent authors the watch as a mission graph. The delivery target
is explicit in the mission; it is never inferred from a caller's terminal environment. The one
standalone watch is a seat's watch on a GitHub issue or pull request (`st gh watch`, below).

The subscription key includes the provider kind, provider locator, selected fields, target, and delivery type. An exact retry returns the same subjects.

## Graph shape

The command publishes this graph shape:

```kdl
resource "github/compoundingtech/st2/pull/403" {
  kind "vcs.pull-request"
}

mission "resource-watch/github/compoundingtech/st2/pull/403/KEY" state="ready" {
  goal "Observe one resource and send its selected changes."
  observer "watch" {
    resource "resource/github/compoundingtech/st2/pull/403"
    provider "github.pull-request"
    locator "compoundingtech/st2#403"
    field "head"
    field "state"
    field "review"
    field "checks"
  }

  subscription "watch" {
    observer "observer/watch"
    to "agent/example.worker"
    on "head"
    on "state"
    on "review"
    on "checks"
    delivery "message"
  }
}
```

The resource begins unbound. The resource stores normalized external facts after its first observation.

The mission run owns the observer and subscription.

The returned subjects use `observer/RUN/watch` and `subscription/RUN/watch`.

The subscription stores the selected changes and delivery intent. The agent declaration does not change.

A provider locator is an opaque provider value. st does not assign meaning to it outside the registered provider.

The `github.ref` locator is `OWNER/REPOSITORY@BRANCH`. Branch names can contain `/`. For example:

```kdl
resource "github/shareup/app-web/ref/one-space-at-a-time" {
  kind "vcs.ref"
}

observer "one-space" {
  resource "resource/github/shareup/app-web/ref/one-space-at-a-time"
  provider "github.ref"
  locator "shareup/app-web@one-space-at-a-time"
  field "head"
  field "ancestors"
}
```

`ancestors` does not include the selected branch itself. The provider sorts and deduplicates the returned refs.

The GitHub providers read `GH_TOKEN` first and `GITHUB_TOKEN` second. They then try `gh auth token`.

It uses public API access when no authenticated token is available.

## Observation without delivery

An observer does not need a subscription. This form records one resource without sending a message:

```kdl
resource "workspace/config" {
  kind "filesystem.file"
}

mission "observe-config" state="ready" {
  goal "Keep the configuration metadata current."
  observer "config" {
    resource "resource/workspace/config"
    provider "local.file"
    locator "/work/project/config.toml"
    field "status"
    field "content_hash"
    field "size"
    field "mode"
  }
}
```

The mission run owns the observer. Mission cancellation stops the observer.

An internal declarative refresh operation requests an immediate observation and waits for that exact
attempt. It returns `changed=false` when the provider confirms the same facts. That success adds no
resource claim.

The refresh operation records `observer.refresh-requested`. Its matching `observer.observed` receipt completes the request.

An observer whose revision met a permanent error, such as a rejected observation, is not polled again
on that revision. A refresh request still polls it once, and the resulting `observer.state` carries
the request's attempt, so the observer and its subscriptions can recover without a new revision.

## Provider contract

A registered provider converts one locator into normalized resource fields.

The provider returns an unchanged result or one complete observation. A partial response cannot replace the last good observation.

The GitHub repository provider reads what changed since its cursor. Its first poll reads every open
item, up to 10 pages; a larger open listing fails the observation instead of recording part of it.
After that, it reads the issues listing for items updated since its watermark, which also names
each closed pull request's merge, and the repository's comments updated since a second watermark.
A comment watermark starts at the first poll, so st does not read old comments. Each read is
conditional and its URL changes only when something changed, so an idle repository answers 304
and costs nothing. A read longer than 10 pages records what it read and continues at once.

Pull request heads, checks, reviews, and merge-queue state come from one GraphQL query for every
open pull request. GraphQL has no conditional request and spends its own hourly budget, so the
query runs at the first poll, when the issues listing shows an open pull request changed, while a
pull request has pending checks, failed required checks that a rerun may fix, or a place in the
merge queue, and otherwise every 15 minutes, but
never sooner than a minute after the last one unless a refresh asks. A change seen within that
minute is remembered, and the first poll after it reads. Every observer of the repository on a host
shares the answer. Submitting a review moves the pull request's update time, so the issues listing
shows it.

With each GraphQL read, the provider reads the rules of each open pull request's base branch:
`rules/branches/BASE` and `branches/BASE`, conditionally and with read access. A 403 or a 404 from
either leaves the other; when GitHub shows neither, every check counts. A refused rule never fails
the observation.

A reaction changes no update time. With `reactions`, the provider also reads the open issues
listing, which names each open item's reactions, and the newest 100 comments for theirs.

The cursor holds both watermarks and the version of each listing whose items were already
recorded, so an unchanged listing returns nothing to record. A cursor from an older build holds
neither, so its next poll is a first poll.

The provider also records the repository's numeric ID. A renamed repository answers its old locator
through a redirect with the same ID, so every item keeps its identity. A different ID fails the
observation. An item is identified by its number within the observed resource.

Builds before item resources kept every item in the repository facts. The first observation
without them records each listed item that had no resource, closes each pull request that was open
there and left the open listing, and records the repository facts without the items. A delivery
compares an item against that older listing, so the upgrade starts no review and no triage.

The provider can use a webhook, a stream, or a conditional request. A conditional provider returns one next-check deadline.
An observer's `every` replaces that deadline, except when the provider asks to continue within a
second, as a listing longer than one read does.

The daemon records that deadline as a one-shot wake. It does not run a periodic discovery sweep.

Conditional requests use provider cursors such as an ETag. Cursors are local progress state, not resource facts.

The daemon keeps each next-check deadline and cursor in local scheduler memory. A daemon restart performs one immediate observation.

The provider applies bounded retries and backoff. It records authentication, rate-limit, and transport failures on the observer subject.

A rate-limited observer waits for the reset GitHub names and asks a person only when the limit
outlasts that reset by a minute. A rejected token or a repository the token cannot read asks a
person at once. Each item closes when the observer observes again.

Every observer on every host shares the token's hourly GitHub budget, as do `gh` and CI. The daemon
counts each GitHub request against the observer that sent it, and keeps the counts and the budget
from GitHub's latest rate-limit headers in memory. `st doctor` shows the remaining budget, how much
of the current window this host's observers spent, and each observer's requests in the last hour
and since the daemon started. A 304 answer to a conditional request is free, so it is counted
apart. Request counts are not resource facts: an unchanged listing records no claim.

Each GitHub request times out after one minute, so a connection that never answers cannot hold its
observer.

An unchanged failure creates no new claim. A later success replaces the complete observer health state and clears the old failure reason.

An observer checks its declared fields. Its effective field set also includes the union of its subscription fields.

st does not fetch once for each target. A subscription update can expand or reduce the observer field set.

## The intake pipeline

A repository observer's items reach whoever owns them, each once. Nothing reaches a person
directly: what is headed for a person goes to an agent, which groups, summarizes, and asks.

```kdl
subscription "curate" {
  observer "observer/repository"
  on "pull_requests"
  on "issues"
  on "mentions"
  mention "orchid-login"
  to "agent/example/curator"
  delivery "message" {
    every "30m"
    owner "message"
  }
}
```

`owner "message"` routes an item that a live agent owns to that agent as one message, and the
delivery does nothing else with it. An item's owner is its `opened_by` agent while that agent's
declaration is live. Otherwise it is an agent of a live mission run that opened the item
(`opened_by_run`) or published its pull request resource, preferring the agent working on one of
the run's steps. An item that no live agent owns takes the delivery's usual path. A review or
triage delivery (`delivery "mission"`) accepts `owner "message"` too.

`every` batches a message delivery: each observation records its new pull request heads, new
issues, and new mentions as one `subscription.batched` claim, and the declaring host sends what
collected as one message at most once per interval, and nothing when nothing arrived. Each line
names the repository, number, kind, title, link, who, why it is headed for a person, and its
delivered marker. `subscription.batch-sent` records the newest batch each message covered.

A mission delivery batches with `text "NAME"` instead: one run per observation receives the same
entries as JSON in its text input, with the repository resource as its resource input.

Each `mention` names a GitHub login whose mentions the subscription hears; mentions arrive only
through a batched delivery. A mention is new when a recorded item did not know it and it was made
no earlier than five minutes before the item's previous observation. A new item's own mentions,
such as a body that copies someone in, go with the item itself. Turning mentions on, reading a body
again, or an edit to an old comment never reports an old mention, and neither does a login's
mention of itself. The baseline delivers nothing.

Each item has a stable delivery key: the subscription's local name, the repository ID, the item,
and its head or the mention. An owner's message takes its subject from the key, and a batch
records the keys it carries, so a repeated observation, a replacement watch, or a daemon restart
delivers nothing more. A review or triage request keeps the key as before.

## A seat's GitHub watch

`st gh watch OWNER/REPO#N` gives the seat that runs it one watch on that issue or pull request. The
seat wakes once for each new comment or review on it, each time the required checks on its current
head move into pass or fail, and a last time when it closes or merges, which ends the watch.

```kdl
subscription "watch/acme/garden/12/example/planner" {
  observer "observer/github/acme/garden"
  to "agent/example/planner"
  on "comments"
  on "issues"
  on "pull_requests"
  delivery "watch" { item 12; since "2026-10-03T14:00:00.000Z"; until "2026-10-03T18:00:00.000Z" }
}
```

The daemon declares the watch for the seat, which owns it; no mission run does. Watching a thread
twice keeps the one watch and takes the new deadline. `st gh unwatch` ends a seat's own watch, and a
person ends any seat's with `--agent`. `st gh ls` lists a seat's watches, running and ended in the
last day, and `--all` every seat's.

Every watch of a repository, from any host, uses one standing observer,
`observer/github/OWNER/REPO`, which no mission run owns. The first watch of the repository declares
it on its host, which polls it every 30 seconds; the host stops it once no running subscription
uses it. No issue or pull request has a poller of its own.

The standing observer records into `resource/github/OWNER/REPO`, unless another observer of the
repository runs: then it records into that observer's resource and keeps that resource from then
on. Subscriptions are grouped by resource, so a poll by either observer delivers to the
subscriptions of both, and every item has one resource.

A mission subscription can name a standing observer too, as an intake does:

```kdl
subscription "pull-request-reviews" {
  observer "observer/github/acme/garden"
  on "pull_requests"
  delivery "mission" { mission "acme/review"; resource "source"; workspace "/srv/reviews" }
}
```

The run declares the standing observer when it starts, and its host keeps it running while the
subscription runs. Cancelling the run stops its subscriptions, never the observer, which stops
only once nothing uses it. To move an intake from an observer of its own onto the standing observer,
revise its run so its subscriptions keep their names and name the standing observer instead, and
drop its own observer. The revision declares the standing observer before the old observer stops,
so the standing observer takes the old one's resource; an observer that never recorded anything
starts from the cursor of the observer that last recorded into its resource. Item facts, delivery
keys and pending requests stay where they were, so the move delivers nothing twice and misses
nothing.

The write that records an observation decides each watch's wakes from its item's prior facts:

- each `recent_comments` entry the item did not know is one wake, when it was made no earlier than
  five minutes before the watch began, so two comments between polls are two wakes; an edit keeps
  its entry and wakes nobody;
- a move of `required_checks.state` into `pass` or `fail`, or a new head first seen there, is one
  wake; a new head starts over, and a rerun that fails again on the same head wakes again;
- the item closing or merging is the last wake, and `subscription.watch-ended` records it in the
  same write, so nothing follows it. Only that watch ends.

Each wake is one `message.sent` from the observing host to the seat, in prose: who did what where,
with the link. Its subject comes from the watch, when it began, and the event, so a repeated
observation or a restart sends nothing twice, and the baseline sends nothing. When the seat's host
delivers a wake about a comment or review, it reads the text from GitHub by its ID and shows the
first 600 characters; the text is never stored in the graph.

A watch survives a seat's stop and start, suspend and resume, and daemon or host restarts. Wakes
queue as ordinary mail while the seat is stopped. The repository observer keeps polling while
any watch is alive, including a stopped seat's watch, and stops when the last watch ends.

The host that declared a watch ends it without a wake when the seat is retired (its declaration
is removed) or explicitly starts a fresh conversation (`fresh-context`). Ending a mission run
or stopping its seat does not itself remove the watch. A deadline gives one final wake and ends
the watch even while the seat is stopped. Each ending also stops that watch's declaration.

Every fleet agent posts as one GitHub login, so a login cannot say which seat wrote a comment.
`st gh comment OWNER/REPO#N --body-file FILE` posts through the seat's daemon, records the new
comment's GitHub ID as the seat's (`github.posted` on `github-post/OWNER/REPO/KIND/ID`), and
watches the thread unless `--no-watch`; `--review approve|request-changes|comment` posts a pull
request review instead. `st gh own URL` records a comment or review posted some other way, and
refuses one whose author is not the login this host posts as. The first seat to record an ID keeps
it.

A seat never wakes for what it recorded. The observing host writes no wake for it when the record
has reached it, and the seat's own host, which knows its posts at once, withdraws any wake that
arrived first and holds a thread's wakes while the seat's post is in flight. No one else waits:
every other seat's wake about a recorded comment names the seat that posted it, and a comment from
the shared login that no seat recorded wakes every watcher, since a person may share the login.

## Retention

Each item resource keeps its latest state. An observation replicates only a change, and a checkpoint
drops each observer observation that a newer one replaced. It keeps a version that a subscription
request, a message, or a mission run input names. Comment bodies and other high-volume history stay
out of the replicated log.

## Change and delivery rules

The first successful observation establishes the baseline. It sends no update message and starts no mission.

A mission delivery pins the observed resource claim as a run input. A bare mission name uses its
current ready revision when the request starts; `MISSION@REVISION` keeps an exact revision.

```kdl
subscription "new-ready-pull-requests" {
  observer "observer/repository"
  on "pull_requests"
  delivery "mission" {
    mission "review/pull-request"
    resource "pull-request"
    workspace "/work/pull-request-reviews"
    requester "agent/example/repository/standing/owner"
  }
}
```

The repository provider creates one mission request for each newly discovered issue and each new
ready pull request head. A pull request is reviewed at a head only when it first appears open and
ready, when a draft becomes ready, or when a new head replaces a known one. A closure, a merge, a
reopening at the same head, a title edit, and a field that an older build did not record never
request a review. A known item whose head was never recorded gets a baseline head, not a review. The run input pins that item's exact discovery claim. Requests for the
same mission, stable local subscription name, item, and PR head are remembered across replacement
intake runs; a title or state change at the same head cannot start another review. Distinct local
subscription names can still start separate workflows. An issue number is triaged once per
workflow. The subscription request carries a stable delivery key and the chosen run records its
exact mission revision. An old request without a delivery key is matched by its pinned discovery
claim. Before starting a PR review, the reconciler also checks whether a matching mission-run PR
resource is already covered by that run's human review gate; if so, it cancels the redundant
request with a reason naming the authoring run.

The first listing that records a repository ID is a baseline for its collections. Earlier facts
came from a first-page read, so the older items it adds were missed, not opened.

Every new item gets a durable mission request. Requests wait in observation order and start as
capacity becomes available under the mission's `concurrent-runs` limit. The run's gates govern
execution after it starts. A capacity retry keeps its place ahead of newer requests; the daemon
retries automatically with bounded backoff. No burst size asks a person to release requests.

The queue is visible with:

```sh
st missions requests subscription/NAME
```

It shows open requests as `pending`, oldest first. A person can cancel a request with
`st missions cancel-request REQUEST --as person/operator --reason "the work is no longer needed"`.
A cancelled request never starts.

On upgrade, the reconciler automatically moves existing held requests into this queue. It cancels
requests for superseded heads, closed or merged pull requests, or pull requests back in draft,
with a reason. Current requests start as capacity allows. Stored attention items from the old
five-request cap close automatically, including while the queue is waiting for capacity.
`st missions release` remains available for legacy held requests; automatic intake does not need it.

An optional `requester` names one exact agent or person as the run's requester.

A draft pull request creates a resource and no request. Its first ready observation creates one mission request.

A pull request review request that has not started, including a legacy hold or one waiting for
capacity, starts only while its head is the open pull request's current head. When the pull request
resource shows it closed, back in draft, or at a newer head, the reconciler records
`subscription.mission-request-cancelled` with the reason instead of starting it. The newer head has
its own request.

A request recorded before item claims carried `head_sha` is matched by the head that its item
claim's cited repository listing named.

The GitHub issues endpoint also returns pull requests. The provider removes those records from the issue collection.

The delivery cites the discovery claim. A retry uses the same run. A capacity limit leaves the request pending.

A request can name a mission revision, an owner run, or an input claim that has not reached this host
yet. That request also stays pending, and it starts once replication delivers the claim. Meanwhile
the subscription records a `reconcile.fault` naming the request and the cause.

A request that lacks a field records `subscription.mission-failed`, and so does a request that
cannot start for any other reason. A failing request never holds back the subscription's other
requests or another subscription.

A later observed field change creates one `resource.observed` claim. An unchanged observation creates no resource claim.

A scheduled unchanged observation creates no durable observer claim. A manual refresh creates one `observer.observed` receipt for its exact attempt.

Each subscription that selected a changed field creates one message. Its stable key uses the observation claim and subscription subject.
For a repository observer, the message lists up to 20 changed items with their facts, cites their
claims, and counts them in `items_changed`.

An optional `when` block changes this from delivery on every selected change to delivery on a
false-to-true predicate transition:

```kdl
subscription "green" {
  observer "observer/pull-request"
  on "checks"
  when {
    every "checks" {
      field "status" "is" "completed"
      field "conclusion" "is" "success"
    }
  }
  to "agent/example/cos/standing/cos"
  delivery "message"
}
```

The condition reads the observer's current resource facts, so its predicates omit a subject. It
accepts `field`, `every`, and `not-every` with the same `is`, `starts-with`, and `contains` operators
as graph predicates. `every` and `not-every` apply all nested field predicates to each item in the
selected list.

The baseline never delivers. A later selected-field change delivers only when the prior facts did
not satisfy the condition and the new facts do. Further changes while the condition remains true do
not deliver again. If it becomes false and later true, that new transition delivers. The same rule
applies to message and mission delivery.

A maintained harness driver delivers that message through its native channel or durable inbox.
Generic terminal input is not a delivery boundary.

A daemon restart can repeat an external request. It cannot create a duplicate observation or message.

The normalized observation is the resource authority. A raw provider response can be immutable evidence, but it cannot accept a separate resource mutation.

A missing delivery target creates a warning and keeps the subscription pending. It does not block the observer or another subscription.

The subscription becomes active when its delivery target appears.

## Lifecycle

Cancelling the watch mission run stops that subscription. It does not remove the resource or other
watch runs.

Cleanup stops the owned observer and subscription before the watch run becomes cancelled.

A run revision that no longer declares an owned observer or subscription stops it the same way.
A stopped subscription records `subscription.mission-request-cancelled` for each request it has not
started. Runs it already started continue. Only the host that declared a subscription starts its runs.

The GitHub pull request provider observes the final merge or closure change. The subscription remains active until an explicit unwatch in the MVP.

A `when` condition gates delivery; it does not stop the subscription. Use an explicit unwatch or
mission cancellation to stop it.

## Acceptance proof

- An exact command retry creates no duplicate graph subject.
- The first observation creates a baseline and no message.
- An unchanged provider result creates no claim and no message.
- A selected scalar field change creates one observation claim and one message.
- A replacement intake over older discovery history requests no review for a closed or merged pull
  request, or for an item whose only change is a field the older build did not record.
- A pending or held review of a pull request that closed or moved to a newer head is cancelled with
  its reason, and only the current head is reviewed.
- A new pull request names the one agent on the observing host whose workspace has its branch.
- Each new repository collection item creates one resource and one mission request; a new ready PR
  head creates one more, and an unchanged head or issue does not replay across intake restarts.
- A renamed locator and a first complete listing create no mission request.
- A burst of twenty new items queues every request and starts all twenty in order as mission capacity allows.
- New requests cannot pass an older request waiting on a capacity retry.
- Legacy held requests migrate automatically; current requests start and stale requests cancel with a reason.
- Stored held-request attention closes even while the queue waits for capacity.
- A draft-to-ready transition creates one mission request.
- One repository observer records each item as its own resource; an unchanged poll records no
  claim, and a poll that read only a comment keeps the item's other facts.
- An unchanged repository costs only 304 answers, and GraphQL runs again only while a pull request
  settles or after 15 minutes.
- An item first seen through a comment, or created before the watermark, is not new and asks for
  no triage.
- A mention subscription hears a mention and not a finished check.
- A new head of a pull request that a live agent opened reaches that agent as one message and
  requests no review; one whose opener is gone requests a review.
- New items and mentions reach a batched delivery's agent together, at most once per interval and
  never when nothing arrived; an owned item reaches its owner instead.
- A new item's own body mention, an old mention, a self-mention, an unnamed login, and the
  baseline reach nobody, and no delivery asks a person directly.
- Pull requests from the GitHub issues endpoint do not create issue resources.
- An unselected field change creates an observation claim and no message for that subscription.
- A daemon restart creates no duplicate message.
- Two subscriptions share one observer and receive separate messages.
- A missing target stays pending without blocking observation.
- A provider failure changes observer health without changing the last good resource facts.
- The local file provider reports metadata and never reports file content.
- A direct observer without a subscription records observations and sends no message.
- A manual refresh waits for its exact attempt and reports an unchanged success without a new resource claim.
- A GitHub ref observation records the selected branch head and its sorted merged branch refs.
- A conditional subscription sends once on each false-to-true transition and stays quiet while the condition remains true.

## Observer process isolation to evaluate

The daemon can schedule observations and accept their results.

An external worker process can run each provider operation. This boundary can keep a provider failure outside the daemon.

The design must test crashes, hangs, time limits, bounded results, and failed observation records before it selects this boundary.
