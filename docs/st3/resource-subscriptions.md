# Resource subscriptions

Status: current design.

## Outcome

An agent or a person can request a message or mission when selected external facts change.

The request is durable graph state. A supervised observer checks the external resource without using an agent turn.

The providers observe one GitHub pull request, one GitHub repository, one GitHub branch ref, or one
local file.

The GitHub provider supports `head`, `state`, `review`, and `checks`.

The repository provider supports `pull_requests` and `issues`. It retains discoveries and filters draft pull requests.
Each listed pull request records its `number`, `url`, `title`, `head` SHA, `branch`, `author` login,
`state`, and `draft`. A retained pull request that leaves the open listing becomes `closed`; an open
listing cannot tell a merge from a closure.

The `github.ref` provider supports `head` and `ancestors`. `head` is the selected branch's commit SHA.
`ancestors` contains the full `refs/heads/NAME` name of every other repository branch whose head is
reachable from the selected branch.

Each newly ready pull request becomes one `vcs.pull-request` resource. Each new issue becomes one `vcs.issue` resource.
A pull request resource follows its item: a new head, a closure, and a return to draft each record
one more observation of it, with `head_sha`, `branch`, `author`, and `state`.

Agents open pull requests with a shared GitHub identity, so the author login cannot say which agent
opened one. When a pull request appears or moves to a new head, the observing host looks for the
agent on that host whose workspace has the pull request's branch checked out. When exactly one
agent has it, the listing records that agent as `opened_by` and its mission run as `opened_by_run`.
A pull request keeps an opener once named, so a reviewer or fixer that later checks out the branch
does not take it over. When no agent is named, a mission run that published
`resource/mission-run/RUN/pull-request` for the pull request becomes its `opened_by_run`. A review
mission routes its findings to that agent or run.

The local file provider supports `status`, `path`, `content_hash`, `size`, `mode`, and `reason`. It never returns file content.

## Authored watch operation

A planner or authorized producing agent authors the watch as a mission graph. The public CLI does
not expose a standalone resource-watch mutation. The delivery target is explicit in the mission;
it is never inferred from a caller's terminal environment.

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

The GitHub repository provider reads every page of the open pull request and issue listings, up to
10 pages each. A larger listing fails the observation instead of recording part of it, because a
partial listing makes older items look new later. The provider also records the repository's
numeric ID. A renamed repository answers its old locator through a redirect with the same ID, so
every item keeps its identity. A different ID fails the observation. An item is identified by its
number within the observed resource. A listing never removes a previous item or a field that the
observation did not request.

The provider can use a webhook, a stream, or a conditional request. A conditional provider returns one next-check deadline.

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
    requester "agent/fleet/repository/standing/owner"
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

One observation requests at most five mission runs for one subscription. It records the rest as
held requests and asks `person/operator` once to decide them. A person lists, releases, or cancels
requests:

```sh
st missions requests subscription/NAME
st missions release REQUEST --as person/operator --reason "the review is needed"
st missions cancel-request REQUEST --as person/operator --reason "an old issue resurfaced"
```

A released request starts like any pending request. A cancelled request never starts.

An optional `requester` assigns run revision authority to one exact agent or person. The requester still needs its matching `mission-authority` rule.

A draft pull request does not create a resource. Its first ready observation creates one resource and one mission request.

A pull request review request that has not started, including one waiting for capacity or held for
a person, starts only while its head is the open pull request's current head. When the pull request
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
  to "agent/fleet/cos/standing/cos"
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
- One observation holds each request beyond its first five for a person.
- A draft-to-ready transition creates one pull request resource and one mission request.
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
