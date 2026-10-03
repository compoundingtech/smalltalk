# 0001 Runtime completion through clap's dynamic engine

Status: accepted

Date: 2026-09-30. Johannes decided this in an interview. The axe decision records q1–q9 hold each
question with its options and evidence.

## Context

`st completions` produced static clap scripts. Entity arguments such as the subject of
`st terminals attach` fell back to file completion.

## Decision

| Handle | Question | Choice |
| --- | --- | --- |
| q1 | Mechanism | clap dynamic engine (`CompleteEnv`) |
| q2 | Scope | every entity kind with a client list API |
| q3 | Descriptions | curated per kind, joins allowed |
| q4 | Matching | prefix completion plus CLI short-name resolution |
| q5 | Relevance and failure | filtered per command, silent failure |
| q6 | VRS home | `docs/st3/cli-completion/` |
| q7 | Stub binding | store path, generated at Nix build time |
| q8 | Short-name rule | four-step ladder with ambiguity errors |
| q9 | TAB deadline | 300 ms |

## Options

| Handle | Rejected option | Why rejected |
| --- | --- | --- |
| q1 | Static scripts with a hidden `__complete` patch layer per shell | Three hand-maintained shell layers over generated scripts break easily on clap upgrades |
| q2 | Terminals only; terminals and agents | Inconsistent CLI; other entity arguments keep file completion |
| q3 | Fields of one list call only | Terminal descriptions would show only state and host |
| q4 | Substring completion | zsh's default matcher drops it; shells would behave differently |
| q4 | Prefix only | Long subjects need several TABs and no short form works |
| q5 | Whole kind | Offers values the command then rejects |
| q6 | A subsystem of the st2 VRS in `docs/vrs/` | Places st3 behavior under the st2 vision |
| q7 | `st` looked up on `PATH` | A different `st` on `PATH` breaks completion |
| q8 | Exact last segment only | `a13d`-style prefixes of generated IDs do not resolve |
| q9 | 150 ms; 1 s | 150 ms empties results under heavy load; 1 s makes a stall noticeable |

## Evidence and Argument

- A prototype on dev3 (2026-09-30) used clap_complete 4.6 `unstable-dynamic`. It returned live
  terminals with descriptions in fish (`complete -C`), zsh, and bash, and it completed through the
  `pty` alias.
- hyperfine, load average 167: an entity TAB took 22 ms, a subcommand TAB 2 ms, and a list call to
  an unreachable endpoint with `--daemon-wait 0` failed in 10 ms. 300 ms leaves about 10x headroom.
- zsh with its default matcher dropped candidates that did not share the typed prefix; fish kept
  them. Prefix completion plus resolution at run time gives the same result in every shell.

## Consequences

- Each clap_complete upgrade needs a review of the unstable engine API (T01).
- Every TAB starts st once (T03). `main` answers completion before it loads config or builds the
  async runtime.
- A bare word on the resolving commands needs one list call before the command acts. The literal
  meaning remains when the daemon does not answer.
