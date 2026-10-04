# When something is wrong

Start with the smallest question: is the daemon reachable, is the seat ready, or is the work waiting on a dependency? These commands are read-only and use the invented garden from [getting started](getting-started.md):

```sh
st doctor
st service status
st missions show mission-run/garden/first-note/one
st agents show agent/garden/worker
st agents queue agent/garden/worker
```

| What you see | Next check |
| --- | --- |
| Daemon unavailable | `service status`; the local socket and the user service on that machine. |
| Seat starting or waiting | Attach to its terminal and finish login, trust, or permission prompts. |
| Seat stopped, failed, or missing from the list | Show its exact ID and history; read its stop reason before starting it. |
| Mission queued or blocked | Its predecessor run, step dependencies, human gate, and seat queue. |
| Message waiting or delivery stale | The recipient's local doctor and delivery assessment; inspect the message receipt. |
| Remote member last seen | Check the encrypted route; a sleeping member is normal and catches up on return. |

To inspect a stopped seat and why it stopped:

```sh
st agents ls --all
st agents show agent/garden/worker --json
st subject history agent/garden/worker --limit 20
st terminals peek agent/garden/worker
```

The last terminal screen can explain a login, crash, or provider limit; the graph says whether the stop was requested, the owner ended, a suspension is active, or the runtime failed. A stopped seat can retain a historical terminal, and a seat with no recorded terminal may have nothing to peek.

## Check usage without starting another turn

```sh
st usage --hours 24 --by agent
st usage --hours 24 --by mission
```

These are observed token totals. Cost is an API-equivalent estimate, and unpriced models are shown as unpriced. Account limits and stop policies can explain why a seat stopped; the seat's reason is more useful than repeatedly restarting it. See [model accounts](st3/accounts.md).

## Recover the cause

Finish a first-run prompt by attaching, then detach with **Ctrl+\\**:

```sh
st terminals attach agent/garden/worker
```

After fixing its dependency, restart a stale running seat; explicitly start an intentionally stopped one. Resume a suspended one instead. [Seat lifecycle](seat-lifecycle.md) gives the commands and their different effects. Retry failed work through the graph after correcting its cause; preserve the failure evidence rather than completing a step just to make the display green.

For a Claude seat with stale delivery, check `st claude-channel status`, then attach and use Claude's `/mcp` menu. If `plugin:st-channel:st` says failed, select it and **Reconnect**. The v0.3.4 rehearsal needed this after some launches; delivery then became `current` and queued work ran. A ready installation does not by itself prove that a seat's channel connected.

If doctor reports an operational contradiction, inspect the bounded repair plan:

```sh
st repair dry-run
```

Apply only the exact reviewed token with the procedure in [operational repair](st3/operational-state/README.md#doctor-and-operational-repair). A changed plan needs a new review; repair appends transitions instead of deleting history.

## What the watchdog does

The runtime watches process liveness, readiness, claim leases, and message delivery; `doctor` reports those facts. A fleet may also run an **operations watchdog mission** that inspects health, groups faults, and routes repairs to an operations agent. That mission is an operating policy, not something every new installation receives automatically. Look for it under Missions, and read its run when it reports a fault; Home should show only what needs your action.

A watcher cannot fix a missing harness login, choose a product preference, or make an expired provider allowance return. Follow the recorded cause, and ask a person through a structured request when only that person can settle it.

See [CLI tour](st3/cli-guided-tour.md), [operational state](st3/operational-state/README.md), and [replication](st3/replication.md) for deeper diagnosis.
