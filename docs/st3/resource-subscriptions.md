# Resource subscriptions

Status: current design.

## Outcome

An agent or a person can request a message or mission when selected external facts change.

The request is durable graph state. A supervised observer checks the external resource without using an agent turn.

The providers observe one GitHub pull request, one GitHub repository, one GitHub branch ref, or one
local file.

The GitHub provider supports `head`, `state`, `review`, and `checks`.

The repository provider supports `pull_requests` and `issues`. It retains discoveries and filters draft pull requests.

The `github.ref` provider supports `head` and `ancestors`. `head` is the selected branch's commit SHA.
`ancestors` contains the full `refs/heads/NAME` name of every other repository branch whose head is
reachable from the selected branch.

Each newly ready pull request becomes one `vcs.pull-request` resource. Each new issue becomes one `vcs.issue` resource.

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

A provider locator is an opaque provider value. st3 does not assign meaning to it outside the registered provider.

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

## Provider contract

A registered provider converts one locator into normalized resource fields.

The provider returns an unchanged result or one complete observation. A partial response cannot replace the last good observation.

The provider can use a webhook, a stream, or a conditional request. A conditional provider returns one next-check deadline.

The daemon records that deadline as a one-shot wake. It does not run a periodic discovery sweep.

Conditional requests use provider cursors such as an ETag. Cursors are local progress state, not resource facts.

The daemon keeps each next-check deadline and cursor in local scheduler memory. A daemon restart performs one immediate observation.

The provider applies bounded retries and backoff. It records authentication, rate-limit, and transport failures on the observer subject.

An unchanged failure creates no new claim. A later success replaces the complete observer health state and clears the old failure reason.

An observer checks its declared fields. Its effective field set also includes the union of its subscription fields.

st3 does not fetch once for each target. A subscription update can expand or reduce the observer field set.

## Change and delivery rules

The first successful observation establishes the baseline. It sends no update message and starts no mission.

A mission delivery starts one exact mission revision with the observed resource as a run input.

```kdl
subscription "new-ready-pull-requests" {
  observer "observer/repository"
  on "pull_requests"
  delivery "mission" {
    mission "review/pull-request@REVISION"
    resource "pull-request"
    workspace "/work/pull-request-reviews"
    requester "agent/fleet/repository/standing/owner"
  }
}
```

The repository provider creates one mission request for each newly discovered item. The run input pins that item's exact discovery claim.

An optional `requester` assigns run revision authority to one exact agent or person. The requester still needs its matching `mission-authority` rule.

A draft pull request does not create a resource. Its first ready observation creates one resource and one mission request.

The GitHub issues endpoint also returns pull requests. The provider removes those records from the issue collection.

The delivery cites the discovery claim. A retry uses the same run. A capacity limit leaves the request pending.

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

The GitHub pull request provider observes the final merge or closure change. The subscription remains active until an explicit unwatch in the MVP.

A `when` condition gates delivery; it does not stop the subscription. Use an explicit unwatch or
mission cancellation to stop it.

## Acceptance proof

- An exact command retry creates no duplicate graph subject.
- The first observation creates a baseline and no message.
- An unchanged provider result creates no claim and no message.
- A selected scalar field change creates one observation claim and one message.
- Each new repository collection item creates one resource and one mission request.
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
