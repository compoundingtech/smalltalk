# st shell completion requirements

## Context

- The st command surface is described in [../README.md](../README.md) and
  [../cli-guided-tour.md](../cli-guided-tour.md).
- The implementation contract is [spec.md](./spec.md). Decisions:
  [.decisions/0001-runtime-completion.md](./.decisions/0001-runtime-completion.md).

## Assumptions

- **A01 Local daemon:** Entity candidates come from the st daemon on this host through its trusted
  local Unix socket. The daemon answers a list request in tens of milliseconds.
- **A02 Supported shells:** People use zsh, fish, or bash.

## Acceptable Tradeoffs

- **T01 Unstable completion engine:** The completion engine is a feature that its upstream marks
  unstable. st pins its exact version and accepts the upgrade work.
- **T02 Bash has no descriptions:** Bash shows candidate values only. zsh and fish show values with
  descriptions.
- **T03 One process per TAB:** Every TAB starts the st executable once.

## Requirements

### Must offer the real choices

- **R01 Live entity candidates:** An argument that names an st entity of a kind the client API
  lists (for example terminal, agent, mission, mission run, work step, attention item, message,
  lane, fleet host, launch, device, subscription, session, person) completes to every current
  entity of that kind.
- **R02 Command-relevant candidates:** Each argument offers only entities its command accepts. For
  example, `st terminals attach` offers running terminals, `st agents stop` offers running agents,
  and `st work retry` offers failed steps.
- **R03 Rich descriptions:** Each candidate carries one line with the facts a person uses to choose
  it, such as state, host, harness, current work, and age.
- **R04 Static surface:** Subcommands, flags, and closed value sets complete as before.
- **R05 No false file candidates:** An argument that does not take a path never offers files.

### Must never get in the way

- **R06 Bounded latency:** One TAB finishes its daemon requests within 300 ms, or offers no
  entity candidates.
- **R07 Silent failure:** When the daemon is unreachable, slow, or fails, completion offers no
  fresh entity candidates and writes nothing to the terminal. A successful terminal suggestion
  cache may be reused for at most 1 s.

### Must accept short names

- **R08 Prefix matching:** Completion matches typed text against the start of the full subject in
  every shell.
- **R09 Short-name resolution:** A command that takes a terminal, agent, mission run, or attention
  item accepts a unique short name (for example `st terminals attach steward`). Resolution sees
  all subjects of the kind. Destructive verbs accept only exact namespace or last-segment names.
  A short name that matches several subjects fails with status 2 and lists them.
- **R10 Literal compatibility:** A word that resolves to nothing, or any word while the daemon does
  not answer, keeps its previous meaning. Bare-word attach must resolve and consult within the
  existing 1 s budget or fail fast; a full subject preserves offline attachment.

### Must install with st

- **R11 Installed stubs:** An installed st package ships zsh, fish, and bash completion that call
  back into the PATH-resolved `st` executable.
- **R12 Printable stub:** `st completions <shell>` prints the same PATH-based stub for manual installation.
