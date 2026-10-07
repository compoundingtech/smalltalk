# GitHub integration

Smalltalk can observe a repository and start finite review and triage missions for new pull requests and issues. A durable seat serves those runs; a curator groups what needs a person before it reaches Home.

Use the worker and workspace from [getting started](getting-started.md), and a repository you are allowed to inspect. Install [GitHub CLI](https://cli.github.com/), then log in as the OS user who runs the daemon:

```sh
gh auth login
gh auth status
st doctor
```

By default, the observer uses the daemon account's `GH_TOKEN`, `GITHUB_TOKEN`, or `gh auth token`. Login must be visible in its login-shell environment. The daemon resolves credentials once and reuses them until GitHub returns HTTP 401 or the selected credential source changes. A rejected credential is invalidated for future requests; refresh attempts back off for 30 seconds, and the original request is never retried, including a comment or review. `GH_TOKEN` takes precedence over `GITHUB_TOKEN`, then `gh auth token`; exported credentials are rechecked through the daemon's existing login-environment snapshot (refreshed on use every 60 seconds), and changes to `gh`'s `hosts.yml` metadata trigger one refresh. A keychain-only change that leaves that file untouched requires a 401 or daemon restart. Agents' `gh` comments, reviews, and pushes use **the person's GitHub account**; an agent seat name is not a separate GitHub identity. Set clear publishing constraints in the work brief.

## Token-file configuration

Choose an explicit token file in the daemon's configuration to take precedence over the
login-shell environment and `gh auth token`:

```toml
[github]
token_file = "/path/to/github-token"
```

The file must contain only one token, with an optional trailing newline. Make it readable
only by the daemon account (`chmod 600`), which must own it. Group and others must have no
permission bits. The final path component must not be a symlink; keep its parent directories
under trusted ownership. The daemon checks permissions at startup and on each
credential acquisition, caches the contents, and reloads when the mtime changes. Atomic file
replacement is detected even when the mtime is preserved. An unreadable, unsafe or malformed
file fails authentication rather than selecting another source.

## Gateway-authorized configuration

To keep the GitHub token at the sekrets gateway, configure the daemon:

```toml
[github]
sekrets_profile = "owner/daemon-gh"
```

Grant the profile to the daemon's node with the `github-api` preset (see
[sekrets](st3/sekrets.md)). The gateway client reads the daemon or gate CLI's Config and
existing node key; no initializer or token handoff is needed. Every daemon GitHub HTTP
request uses the gateway's authorized-request operation, including GraphQL, pagination,
conditional reads and comment/review mutations. The profile path never resolves local
files, exported tokens or `gh auth token`; it never receives the profile token.

A missing gateway, identity, grant or refused URL errors without selecting another source.
The gateway authorizes only `https://api.github.com`, rejects a request's own Authorization,
and adds the profile credential there. It follows no redirect and retries no request. The
client runs off the async worker threads and returns the original status, headers and body,
including 3xx, 401, ETag, Link, rate limits and Retry-After. HTTP errors remain responses;
transport and policy errors remain failures. Request/response limits are 16 MiB/64 MiB,
with the gateway's 60-second HTTP timeout. Local API overrides pointing elsewhere are
refused in profile mode. The caller also rejects foreign origins, userinfo, non-default ports
and an Authorization header before the RPC. It refuses streamed bodies and buffered request
bodies above 16 MiB, and checks the 64 MiB response bound before constructing its HTTP response.

Setting `github.sekrets_profile` together with `github.token_file` is a configuration error.
With neither option set, the existing environment/CLI source precedence and cache apply.

## Daemon GitHub callers

The daemon's shared authentication and send path covers these API requests:

| Caller | Requests |
| --- | --- |
| `github.repository` | Repository metadata, issues, pulls, comments, reactions, rules and branch protection via REST; pull-request details and resolved nodes via GraphQL. |
| `github.pull-request` and `github.ref` | Pull listings, reviews, check runs, branches and comparisons. |
| `merged` and `ci-passed` gates | Pull merge state, commit heads, check runs and commit statuses. |
| `st gh watch` and wake excerpts | Issue/thread validation, comments, reviews and inline review comments. |
| `st gh comment` and `st gh own` | POST comments or reviews; read a comment/review and the current GitHub login. |

REST listings follow GitHub's `Link` pagination. Conditional reads retain ETags and reuse the
complete cached body and pagination link after a 304. Response status and rate-limit headers
remain available to the caller. Credential invalidation changes future authentication;
it does not retry any of these HTTP requests.

Repository pull-request GraphQL reads start with 20 open pulls per cursor page and preserve
the existing nested review, check and merge-queue selections. GitHub's “Something went wrong
while executing your query” refusal retries the same cursor with half as many pulls, down to
one. Each successful continuation page doubles the next page's size toward 20, but never
back to or above a size already refused during that observation. Each new observation starts
at 20 with no remembered refusal ceiling. Debug logs record each page size; retry warnings record the
failed and reduced sizes. Other failures, or an execution refusal at one pull, fail the
observation without caching any partial result. The complete listing remains bounded at
1,000 open pulls regardless of page size. An observation also permits at most 200 GraphQL
calls, including retries and empty pages with advancing cursors. Exceeding either bound or
receiving a non-advancing continuation cursor fails rather than silently truncating facts.

## Main Performance failure messages

A direct message subscription on `main_performance_failures` adds that field to an existing
`github.repository` observer's scheduled request. No second observer or polling loop is needed:

```kdl
version 2
subscription "main-performance-p0" {
  observer "observer/github/acme/garden"
  on "main_performance_failures"
  to "agent/example/speed"
  delivery "message"
}
```

The existing observer and agent recipient must already be declared, and the observer must be
live. This simple direct-message declaration supports standalone `st apply --dry-run` and
`st apply --as RECIPIENT` when the declared agent recipient publishes as itself. An agent-bound
client must publish as itself; another actor cannot subscribe the recipient. The public event field
is limited to `main_performance_failures`, and an existing registration can only be re-registered
by its original author with the same spec. This path cannot create an observer, start a mission, batch
delivery or publish unrelated top-level runtime declarations. Publish only this subscription;
leave existing declarations and owned sets intact. This narrow field reads workflow-run metadata for
`perf.yml`, branch `main`, event `push`, then verifies the workflow name `Performance`, path
`.github/workflows/perf.yml`, status `completed` and conclusion `failure`. Other events, branches,
workflows, successes, cancellations and timeouts produce no P0 delivery. It fetches no logs or
artifacts and never dispatches a workflow.

Each message has a P0 title and tag, and JSON content with `priority`, `repository`, `run_id`,
`run_attempt`, `head_sha`, `workflow`, `workflow_path`, `url` and the selected status/event/branch.
One message names one failed run attempt. The first successful snapshot for each recipient and
repository establishes a baseline without sending old failures, including when the first listing
is empty. Later attempts and heads have separate delivery keys;
metadata edits, repeated reads, restarts and replacement subscriptions to the same recipient do
not send the same repository/run/attempt/head again. The repository ID keeps that identity across
renames; a missing ID falls back to the name. Re-adding the field after an observation gap
re-baselines the current listing and suppresses failures first observed during that gap.
Baseline and delivery receipts are graph resources committed atomically with the observation and message, using existing checkpoint rules.

This field uses the observer's normal schedule and conditional HTTP cache, including paginated
304 reuse. It reads at most ten pages of 100 runs and fails rather than silently truncates a larger
listing. The shared repository observation must succeed, including any other selected fields;
an existing GraphQL or authentication failure prevents delivery. Deploy supporting daemon source
before registration, then check the observer's successful field observation and message receipts.
An installed subscription alone does not prove healthy delivery. Upgrade participating readers
before enabling this field: older schemas quarantine the new fact field. Removing the subscription
does not erase its historical claims for a later downgrade.

## Prepare finite review and triage work

These examples record results in the graph and leave publication to a later decision. Their goals describe completed results; instructions live in a brief:

```sh
cd ~/st/garden
mkdir -p docs missions
cat > docs/github-brief.md <<'EOF'
Use the discovered source resource named in your step to find the exact GitHub item.
For a PR, read its diff, inspect the affected code, and run the relevant checks.
For an issue, assess reproducibility and identify likely affected files.
Record the verdict, evidence, and any follow-up in the step completion; put a longer report
in a graph document if needed. Use each run's workspace for temporary checkouts.
Do not comment, approve, merge, push, label, or close GitHub items in these missions.
EOF
cat > missions/github-work.kdl <<'EOF'
version 2
mission "garden/review-pr" state="ready" {
  concurrent-runs max=2
  input "source" kind="resource"
  goal "The pull request in ${input.source} has a recorded review of its discovered head."
  constraint "Follow ~/st/garden/docs/github-brief.md."
  step "review" timeout="45m" {
    assigned-to "agent/garden/worker"
    goal "The review verdict and evidence for ${input.source} are recorded in the graph."
  }
}
mission "garden/triage-issue" state="ready" {
  concurrent-runs max=2
  input "source" kind="resource"
  goal "The issue in ${input.source} has a recorded triage result."
  constraint "Follow ~/st/garden/docs/github-brief.md."
  step "triage" timeout="30m" {
    assigned-to "agent/garden/worker"
    goal "Reproducibility, likely affected files, and next action for ${input.source} are recorded."
  }
}
EOF
st apply missions/github-work.kdl --as person/ada
```

## Start repository intake

Choose your actual `OWNER/REPO` when prompted. Publish the child missions **before** the intake that names them:

```sh
cat > missions/github-intake.kdl <<'EOF'
version 2
resource "garden/github-repository" { kind "vcs.repository" }
mission "garden/github-intake" state="ready" {
  input "repository" kind="text"
  goal "New ready pull requests and issues have review and triage runs until intake is retired."
  observer "repository" {
    resource "resource/garden/github-repository"
    provider "github.repository"
    locator "${input.repository}"
    field "pull_requests"
    field "issues"
  }
  subscription "pull-requests" {
    observer "observer/repository"
    on "pull_requests"
    delivery "mission" {
      mission "garden/review-pr"
      resource "source"
      workspace "${ST_WORKSPACE}/reviews"
    }
  }
  subscription "issues" {
    observer "observer/repository"
    on "issues"
    delivery "mission" {
      mission "garden/triage-issue"
      resource "source"
      workspace "${ST_WORKSPACE}/triage"
    }
  }
  step "retire" {
    agentless
    gate "Ada retires repository intake" type="human" {
      reviewer "person/ada"
      question "Stop starting new review and triage runs?"
    }
  }
}
EOF
printf 'Repository to observe (OWNER/REPO): '
read -r garden_repo
mkdir -p "$PWD/github-intake"
st apply missions/github-intake.kdl --as person/ada
st missions start garden/github-intake --id garden/github-intake/main \
  --input "repository=$garden_repo" --workspace "$PWD/github-intake" --as person/ada
st missions show mission-run/garden/github-intake/main
st agents queue agent/garden/worker
stui
```

The first observation is a baseline and starts no work. Subsequent new ready PR heads and issues create runs; drafts wait until ready. Requests are remembered across restarts, and a child mission's capacity limit holds pending requests instead of dropping them. One worker runs these jobs serially; separate review and triage seats let independent work run in parallel.

The `retire` gate keeps the intake alive. Approving it ends intake; it is not approval to publish a PR. When the intake run ends, its observers and subscriptions stop. Cancel it explicitly when you are finished:

```sh
st missions cancel mission-run/garden/github-intake/main \
  --reason 'Finished the intake trial.' --as person/ada
```

## Put a curator in front of Home

Route incoming events and grouped review findings to an agent responsible for deciding what matters. The curator asks the person only for concrete decisions, actionable failures, or requested feedback. Raw PR activity and successful routine checks belong in agent conversations and mission results, not as a pile of person requests.

Subscriptions can batch message delivery with `every "30m"` and `owner "message"`, sending items already owned by an agent to that owner. See [the intake pipeline](st3/resource-subscriptions.md#the-intake-pipeline) for the complete curator declaration. The [structured request examples](talking-to-agents.md#ask-for-a-human-answer) show how a curator presents a decision.

## Land reviewed work through the merge queue

After the review, required checks, and any repository-specific approval are complete, use the repository's merge queue. Enter the exact PR URL you intend to land:

```sh
printf 'Reviewed pull request URL to merge: '
read -r garden_pr
gh pr checks "$garden_pr"
gh pr merge "$garden_pr" --auto
```

For `compoundingtech/smalltalk`, `linux-gate`, `isolation-vm`, and `genie-freshness` are required; [CI operations](ci.md) explains its queue. `--auto` requests landing when GitHub's requirements pass; it is not a substitute for review or proof that a PR has already merged.

See [repository intake examples](../examples/st3/github-intake.kdl), [review/triage examples](../examples/st3/github-intake-work.kdl), and [subscriptions](st3/resource-subscriptions.md) for larger setups and cleanup.
