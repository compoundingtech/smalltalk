# GitHub integration

Smalltalk can observe a repository and start finite review and triage missions for new pull requests and issues. A durable seat serves those runs; a curator groups what needs a person before it reaches Home.

Use the worker and workspace from [getting started](getting-started.md), and a repository you are allowed to inspect. Install [GitHub CLI](https://cli.github.com/), then log in as the OS user who runs the daemon:

```sh
gh auth login
gh auth status
st doctor
```

The observer uses the daemon account's `GH_TOKEN`, `GITHUB_TOKEN`, or `gh auth token`. Login must be visible in its login-shell environment. Agents' `gh` comments, reviews, and pushes use **the person's GitHub account**; an agent seat name is not a separate GitHub identity. Set clear publishing constraints in the work brief.

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
