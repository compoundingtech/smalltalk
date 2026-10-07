# Claude prompt inventory and delivery contract

Inventory target: Claude Code **2.1.292**. The public Linux x64 npm artifact was
verified against its registry integrity digest and executed with `--version`.
Its executable SHA-256 is
`a967e7b1d8b4e47ee421d5433027880347952b0c0857abf880e2c942a4ec93b3`.
The separately installed executable reported 2.1.293; that is not evidence that
every 2.1.292 dialog has been exercised.

The inventory combines the pinned executable's dialog identifiers with the
[vendor hook reference](https://code.claude.com/docs/en/hooks) and
[SDK input contract](https://code.claude.com/docs/en/agent-sdk/user-input).
Current vendor documentation can advance beyond the pinned executable. An
identifier establishes a candidate surface, not its reachable conditions,
exact displayed choices, or a working remote answer channel. Those remain
unknown until a finite native fixture exercises them. This inventory is open;
it does not assert exhaustive prompt coverage.

`implemented` below means the driver has a native mapping. It does not mean
that phone or conversation controls have passed their end-to-end tests.
`available, pending` means a documented native mapping needs implementation
and pinned-version qualification. `unavailable` means this integration has no
supported capture/answer bridge; the actual harness UI remains the safe action.
No category below is answered by terminal keystrokes.

| Kind | Phase and primary evidence | Capture and native response | Current mapping / safe action |
| --- | --- | --- | --- |
| Tool permission, including safety checks under bypass | Live; `PermissionRequest`, safety-check identifiers | Native hook receives tool input; hook decision allows once or denies. Policy can still refuse an allow. | Implemented for tool requests other than questions and plan mode transitions. Exact command or tool input; owning person only. |
| Agent questions and multiple selection | Live; `AskUserQuestionPermissionDialog` | Documented `PreToolUse` updated input or SDK `canUseTool`; retain original questions, answer by question text. | Available, pending. Requires actual choices, multiple selection and free text; never reinterpret as approve/deny. |
| Plan entry and exit approval | Live; `ExitPlanMode`, plan approval text | Documented input hook / SDK route; plan and mode choices differ from tool permission. | Available, pending. Permission-hook capture alone is not a validated plan UI. |
| MCP form elicitation | Live; `executeElicitationHooks` | `Elicitation` supplies schema; accept/decline/cancel; `ElicitationResult` supplies resolution. | Available, pending. Form schema and sensitivity classification required before storage. |
| MCP browser authentication | Live; `UrlElicitationRequired` | Elicitation URL and completion notification; credentials stay in browser. | Available, pending secure flow. Do not persist login URLs containing secrets or credential form answers. |
| Sandboxed command network approval | Live; `SandboxNetworkPrompts` | Delayed `Notification.permission_prompt`; PermissionRequest does not cover this case. | Unavailable response bridge; notification is not proof of exact choices or prompt identity. Use native dialog. |
| Workspace/folder trust, including changed directory | Startup/live; `TrustDialog`, `CdTrustPrompt` | Before trusted hooks can run; no validated per-dialog reply protocol. | Unavailable. Show actual capture only when a supported source exists; use native trust dialog. |
| Bypass mode confirmation | Startup; `BypassPermissionsModeDialog` | No validated capture/response path for startup confirmation. | Unavailable; use native confirmation. Never silently accept it. |
| Development-channel consent | Startup/live; `DevChannelsDialog` | Dialog identified; exact options and external reply contract unqualified. | Unavailable; use native consent. |
| MCP server/device consent and reconsent | Startup/live; `DeviceMcpConsentDialog`, `DeviceMcpReconsentDialog` | Distinct from MCP elicitation; no validated native external reply. | Unavailable; use native server consent. |
| Feedback/rating and follow-up text | Live; `feedbackSurvey`, `callLegacyFeedbackDialog` | Survey identifiers and follow-up paths; no validated capture/reply. | Unavailable. Do not fabricate a rating or opt-in. |
| Memory, plugin, long-context, post-compaction surveys | Live; `memorySurvey`, `pluginSurvey`, `longContextSurvey`, `postCompactSurvey` | Separate survey candidates; exact eligibility and choices unknown. | Unavailable; use actual native survey if shown. |
| Sign-in, account selection and privacy setup | Startup/live; setup/privacy identifiers and auth notification | Auth success can be observed; no general credential-safe form bridge. | Unavailable; use supported native/browser login. No persisted credentials. |
| Usage limit and resume choice | Live; quota notifications | Reset/resume notifications describe outcomes, not a generic answer channel. | Unavailable; preserve real reset/cancel evidence, never infer timeout from absence. |
| Update/model migration | Startup/live; `ThirdPartyModelUpgradeDialog`, `startupModelSwitchDialog`, `launchSnapshotUpdateDialog` | Dialog candidates; exact choices and response protocol unknown. | Unavailable; native update/model UI. |
| Invalid settings/configuration | Startup/live; `InvalidSettingsDialog`, `showInvalidConfigDialog` | May precede installed hooks; no validated per-dialog source. | Unavailable; correct settings through native flow. |
| External instruction includes | Startup/live; `ClaudeMdExternalIncludesDialog` | Distinct trust/consent surface; no validated reply bridge. | Unavailable; native consent. |
| Worktree exit | Live; `WorktreeExitDialog` | Dialog candidate; exact cleanup choices unqualified. | Unavailable; native exit dialog. |
| Remote host/repository mismatch | Startup/live; `TeleportHostUnverifiedDialog`, `TeleportRepoMismatchDialog` | Trust candidates; no validated per-dialog reply. | Unavailable; native verification. |
| Unattended serving, device reenrollment | Startup/live; `UnattendedServingConsentDialog`, `DeviceReenrollDialog` | Consent candidates; no validated capture/reply. | Unavailable; native consent. |
| Browser integration consent | Startup/live; `ChromeAutoEnableDialog`, `getChromeDialog` | Distinct integration consent; no validated reply bridge. | Unavailable; native integration setup. |
| Account-memory edits/writes | Live; account-memory permission/confirmation identifiers | No validated dialog mapping; not assumed to be tool permission. | Unavailable; native dialog. |
| Workflow approval and project continuation | Live; `WorkflowPermissionDialog`, `projectContinuePermissionDialog` | Separate candidates; routing through tool permission unproven. | Unavailable pending native fixture. |
| Connection offers, pairing and remote home settings | Startup/live; Slack consent/sign-in, `abortPairingPrompt`, `RemoteHomeSettingsDialog` | Candidate setup/configuration dialogs; response contracts unqualified. | Unavailable; native setup without persisting secrets. |
| Model failure/fallback, session goal and teammate setup | Live; fallback/session-goal text, input notifications | Candidate waiting states; exact choices/identity/expiry unknown. | Unavailable pending a positive native capture. |
| File/browser/task/diff/layout dialogs | Live; file and task dialog identifiers | Some are nonblocking UI navigation rather than person requests; reachability unqualified. | Unknown. Do not create a home item from an identifier alone. |

The identifier sweep found 541 dialog/prompt/survey/elicitation identifiers,
including helpers and unrelated library names. The table groups evidenced
candidates; unmatched identifiers require classification, not a claim that
the PermissionRequest mapping covers them.

## Common item and authority

`Attention.prompt` is optional. Captured fields are optional too: `kind`,
`content`, `choices` (id/label/consequence), `next_action`, provider/seat/prompt
identity, runtime incarnation, expiry, capability, `can_answer`, state, and
closure provenance. Home and the inline conversation card use the same episode
and `prompt.respond` action. Unsupported mappings have a safe next action and
no buttons. Submitted answers disable controls while awaiting native outcome.

Source records live in a separate indexed local table. Ordinary client claims
cannot manufacture them. The writer transaction resolves the seat's actual
owning person; a second ownership check and the current runtime fence precede
every native answer. Missing ownership records routing as unavailable with a safe next action; it creates no guessed person's item.
Exact content is absent from replicated claims and generic activity events.
The private response socket is never part of client metadata.

Claude's permission hook has no unique per-tool prompt ID. Every live invocation
therefore creates its own random episode and hook ID; identical commands never
share authority. A turn-wide identifier is not a per-tool fence. A local socket
exists only while that invocation is alive.

States are `open`, `answered`, `cancelled`, `timed_out`, `ended`, `unavailable`.
Writing the native hook output records a response handoff, not provider
execution or a separate acknowledgement. A hook's own response deadline can
record `timed_out` and sends a system denial through the hook, with no person
answer or `by` attribution; failed output records `unavailable`. Loss of visibility records `unavailable` without inventing
an answer or timeout. Stopping/replacing the requesting runtime records `ended`.
Terminal states cannot be reopened by a delayed open observation. An uncertain
transport leaves the answer reserved, preventing another potentially duplicated
approval until native outcome settles it.

Recorded prompt closures join the explicit person-scoped history endpoint. Its
authenticated cursor pins the local observation cutoff and captured local clock,
canonical history epoch and separate consumed seek positions. A replicated graph
clock does not bound later local closures. Open readers remain open only; closed rows
have no actions. Local closure records do not enter the canonical history fold,
and the response continues to disclose incomplete, host-local history coverage.

The isolated 2.1.292 fixture proves that the provider consumes the native allow
and deny hook output for a harmless command. Store/API controls cover owner and
runtime fences, one response, expiry, recorded disappearance and mixed history.
These controls do not qualify every built-in safety prompt or terminal UI path.
Remaining qualification includes real pinned-provider safety prompts, terminal
answer observation, inline/phone UI socket controls, cross-host delivery, every
available/pending mapping, and the unclassified candidate set.
Other harness inventories follow the Claude phase.

## Model-free client socket fixture

`cargo run -p st3 --example prompt_fixture -- 90` creates a fresh private graph,
an invented owning person and seat, the real native invocation socket, and the
ordinary client API on a private Unix socket. It exits after 90 seconds (accepted
durations are 10 through 180). The first JSON line reports `fixture_root` and
`endpoint`. Point the designated test client at that endpoint with
`ST3_PERSON=person/ada`; Home and the seat conversation then see one episode.
No installed provider or existing daemon is involved.

To test a native-side response, write the explicit text `approve` or `deny` to
`fixture_root/terminal-answer`. This sends one native response independently of
the client action, so both views must close on the source outcome. Creating
`fixture_root/cancel` aborts the native invocation and reports `unavailable`;
creating `fixture_root/restart` records replacement by `runtime-b` and closes
the old episode. Leaving it unanswered exercises the hook's own deadline five
seconds before fixture exit. Only files inside this fresh fixture affect it.

This seam qualifies client sockets and recorded lifecycle handling. The separate
pinned-provider fixture qualifies consumption of hook output; this server does
not establish how the vendor's terminal UI answers or cancels a prompt.
