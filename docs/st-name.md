# Moving the remaining names from st3 to st

`st` is the public command name. Help, errors, suggested commands, shell completions, terminal
UI labels, and iOS copy use it. The installed `st` link still executes the `st3` binary, and
invoking `st3` remains supported. This change does not migrate stored data or wire formats.

The inventory below groups repeated occurrences by their compatibility contract. A family such
as `st3-work:*` includes every value with that prefix; authored names and historical records are
unbounded and must not be rewritten by a source-code replacement. Paths shown are relative to
the repository unless they start with `~`, `$`, or `/`.

## Inventory and migration order

| Remaining name or family | Who sees it | What renaming it would break | Recommended order |
| --- | --- | --- | --- |
| `st3` executable; `st3-migrate` executable | Operators, scripts, service managers, harness drivers | Existing commands, absolute executable paths, migration scripts, binary discovery, and in-flight drivers. `ST3_BIN` resolves the actual running executable. | 2: introduce new install names with old aliases; retain aliases through a deprecation period. |
| Cargo packages/directories `st3`, `st3-client`, `st3-client-codegen`, `st3-migrate`, `st3-schema`; Rust `st3`, `st3_client`, `st3_schema` imports and `st3_*` / `ST3_*` / `St3*` symbols | Contributors and downstream library users | Workspace manifests, imports, build targets, generated code, Cargo.lock, Nix derivations, and tests. | 3: coordinated source and packaging change after the command aliases are established. |
| Nix `packages.st3`, `packages.st3-migrate`, `checks.st3`, `checks.st3-help`, package `pname`/`mainProgram`, derivation labels; Cargo `CARGO_BIN_EXE_st3` | Nix users, CI, contributors | Flake consumers, CI selectors, package metadata and test executable discovery. | 2–3: add new outputs before retiring old attributes; keep the actual binary available. |
| `clients/typescript/st3-client`, `clients/swift/St3Client`, `St3Client`, generated client symbols and package names | App and SDK developers | Module imports, Swift products, generated model references and consumer builds. | 3: add module/product aliases and regenerate clients together. |
| `.st3/`, `.st3/boot.md`, `.st3/context/`, `.st3-documents/`, `.st3-*` temporary render/doctor files | Agents, workspace owners, Git users | Boot prompts, tracked-file protection, context readers, immutable document lookup, excludes and cleanup. | 4: dual-read old/new paths, choose one writer, migrate existing workspaces explicitly. |
| XDG `st3` directories: `~/.config/st3/config.toml`, `~/.local/state/st3`, data `st3/claude-channel/marketplace`, cache `st3/...`; `st3.sock`, `st3-client.sock` | Operators, CLI/apps, service processes | Config discovery, database continuity, API connection discovery, cache reuse and plugin installation. | 4: explicit move with fallback discovery and rollback; never silently create a second empty daemon. |
| `st3.stdout.log`, `st3.stderr.log`, `st3-replication.stdout.log`, `st3-replication.stderr.log`; temporary `st3-native`, `st3-pty-spawn-locks`, `st3-tmp-*` and `st3-*` test/performance directories | Operators, support tooling, tests | Log collectors, spawn locking, driver registries, fixtures and cleanup paths. | 4: coordinate with services; preserve shared locks during mixed-version operation. |
| `st3.service`, `st3-replication.service`, launchd `com.compoundingtech.st3`, `com.compoundingtech.st3.replication`; `st3-work.scope`, `st3-work-*` scopes | systemd/launchd users, runtime isolation | Service upgrades, uninstall/restart/status, process adoption, scope lookup and cleanup; duplicate services could own one state directory. | 5: stop/transfer/install transaction with rollback and explicit old-service removal. Human-readable descriptions already say st. |
| Runtime environment variables listed below, plus test/soak `ST3_*` variables | Shell users, agents, drivers, CI | Endpoint/identity discovery, claim fences, hooks, message export, budget and gate coordination. | 2 then 4: accept both names, detect conflicting values, keep exporting old names until all consumers migrate. |
| `st3.v1`, `st3.client.v0`, `st3.client.error.v0`, `st3.client.terminal.v0`, `st3.terminal.v1`, `st3.visualization.v0`, `st3.operational-repair.v0`, `st3.cli-purpose.v0`, `st3-migrate-report.v1` | API/SDK users, schema tools, websocket clients, migration/repair consumers | Version negotiation, schema validation, generated clients, stored reports and completion of existing requests. | 6: versioned protocol migration with readers accepting both; retain old fixtures as compatibility proofs. |
| Replication protocol `st3-replication-v1`; headers `x-st3-fleet`, `x-st3-node`, `x-st3-body-sha256`, `x-st3-signature`, `x-st3-request-digest`, `x-st3-person`; websocket `st3.cap.*` | Peers, gateways, authenticated clients | Authentication signatures, peer handshake, person routing and terminal capability negotiation. | 6: negotiate new names across the fleet; do not rename one endpoint in isolation. |
| Hash/domain identifiers listed below | Replication, idempotency and content-addressing code; forensic tooling | Canonical hashes, request deduplication, revision identity, cached operations, immutable envelope verification. A spelling change changes identity. | 7: generally keep indefinitely; change only as a new version with an explicit migration. |
| `__st3/` generated mission prefix; `__st3_loop_round`, `__st3_loop_feedback`, `__st3_loop_item`, `__st3_candidate_index` | Mission authors using expert views, compiler/runtime, apps hiding system missions | Child-run identity, parent-child lookup, loop inputs, UI system filtering and in-flight missions. | 7: teach readers both forms first, create new runs under the new prefix, leave existing runs intact. |
| Runtime tags `st3.subject`, `st3.agent`, `st3.isolation`, `st3.scope-unit`; `st3.redacted` | Runtime adapters, resource clients, inspectors | Process ownership/adoption, terminal association, isolation cleanup and redaction interpretation. | 6–7: dual-read across daemon/runtime/app versions, never discard old ownership tags. |
| Delivery/tag families `st3-message:*`, `st3-message-send:*`, `st3-work:*`, `st3-wake-attempt:*`, `st3-wake-source:*`, `st3-wake-requested-by:*`, `st3-product-wait:*`, `st3-provider-capacity-retry`, `st3-retry-attempt:*`, `st3-to:*`, `st3-sha256:*`, `st3-body-sha256`; `[PING from st3]`, `[st3-delivery:…]`, `st3/context/now.md` | Harnesses, delivery adapters, transcript readers and people reading raw messages | Message recognition, deduplication, wake correlation, retries and transcript filtering. These are retained transport markers, not display labels. | 6: accept both formats throughout the delivery and transcript path before changing writers; retain historical parsing. |
| Claude marketplace `st3`, plugin `st3-channel`, selectors `plugin:st3-channel@st3` / `plugin:st3-channel:st3`, MCP server key `st3`, `st3-claude-channel/`, policy `50-st3-channel.json`, `st3-channel-session` | Claude users, plugin manager, enterprise policy, channel adapters | Installed plugin identity, approvals, MCP registration, managed policy ownership and channel recognition. Status must continue to show actual installed identifiers. | 5–6: separate installation/approval migration, preserving old channel recognition and policy cleanup. |
| iOS persistence `st3.gateway.url`, `st3.tabs.order`, `st3.device.credential`, `st3.projection.v1`; app test variables `EXPO_PUBLIC_ST3_TEST_TAB`, `EXPO_PUBLIC_ST3_TEST_PAIR_LINK` | App storage and QA tooling | Saved gateway, tab order, cached projections and paired credentials. A rename without migration looks like sign-out/data loss. | 4: read old keys, copy safely to new keys, then retire old writers. |
| HTTP user-agent `st3-resource-observer/0.1` | Resource servers and HTTP logs | Server-side allowlists, metrics and request attribution. | 2: announce/change with observer version; retain any required allowlist compatibility. |
| Authored graph IDs such as `agent/st3/...`, `agent/example/st3/...`, `mission/fleet/st3/...`, `agent/st3/reconciler`, document/resource references and `custom.st3.*` kinds | Fleet members, mission authors, operators, external resources | Referential integrity, authority, assigned work, historical evidence and matching rules. UI labels can say ST while IDs remain exact. | 7: migrate declarations through graph operations; never rewrite immutable history or arbitrary user-authored names. |
| Repository paths `docs/st3/`, `examples/st3/`, `evals/st3/`, `scripts/st3-*`, `st3.kdl`, fixtures and evaluation names; literal Git branch `st3` | Contributors, links, automation and release/deploy procedures | File links, include paths, test scripts and actual Git refs. Prose renaming must not rewrite a branch name in fetch/switch/push instructions. | 3 for code paths; keep historical fixtures and Git refs until their owners migrate them. |
| Existing historical prose, stored messages, snapshots, test-only fake IDs (`st3.future.*`, `st3.test`, etc.), and internal variable/function names | Readers of history, contributors and tests | Provenance, deliberately old-version test coverage, snapshots and source references. | Last: update active presentation fixtures when behavior changes; preserve historical evidence and compatibility cases. |

## Runtime environment names retained

The runtime still reads or exports `ST3_BIN`, `ST3_ENDPOINT`, `ST3_PERSON`, `ST3_DAEMON_WAIT`,
`ST3_INCARNATION`, `ST3_TERMINAL_CAPABILITY`, `ST3_DRIVER_STATE_DIR`, `ST3_MESSAGE_ROOT`,
`ST3_RUN_DIR`, `ST3_SUBJECT`, `ST3_TOKEN_BUDGET`, and `ST3_GATE_STATE_DIR`.
`EXPO_PUBLIC_ST3_TEST_TAB` and `EXPO_PUBLIC_ST3_TEST_PAIR_LINK` remain iOS test controls.

Developer and audit scripts additionally use the `ST3_GATE_TEST_*`, `ST3_FRIEND_READY_*`,
`ST3_IDLE_*`, `ST3_SOAK_*`, and `ST3_PERF_STORE` families. Shell-local markers
`ST3_CONTEXT_VARIABLES`, `ST3_ENV_BEGIN`, `ST3_ENV_END`, and `ST3_KDL_VERSION` also remain.
The similarly named Rust `ST3_CHANNEL`, `ST3_MARKETPLACE`, `ST3_PLUGIN`, `ST3_SERVER_NAME`,
`ST3_POLICY_FILE`, `ST3_BODY_MAX_CHARS`, manifest/config constants, and delivery tag constants are code identifiers,
not environment-variable aliases to add blindly.

## Hash domains retained

`st3.claim-request.v1`, `st3.declared-revision-proposal.v1`, `st3.idempotency-cache.v1`,
`st3.idempotency.v1`, `st3.operational-repair-item.v0`, `st3.operational-repair.v0`,
`st3.planning-candidate.v1`, `st3.publish-operation.v1`, `st3.publish.v1`,
`st3.resource-refresh.v1`, `st3.replica-batch.v1`, `st3-replica-envelope-v1`,
`st3-replica-record-v1`, `st3-replication-inventory-v1`, `st3-replication-bucket-v1`,
and `st3-logical-digest-v1` retain their exact bytes, including separators and NUL terminators.

## Sequencing

1. Finish presentation and command documentation; keep `st3` callable. This is the current step.
2. Add aliases for install outputs and environment variables, with compatibility tests.
3. Rename source/SDK/package paths together, with aliases where consumers import them.
4. Migrate local directories, sockets, logs, caches and app keys explicitly and reversibly.
5. Migrate service and plugin registrations without duplicate owners or lost approvals.
6. Negotiate wire, delivery and authentication changes across mixed-version peers and clients.
7. Only then consider generated graph names and hash domains. Preserve immutable history.

Re-audit before each stage with `rg -n -i st3` and `rg --files | rg -i st3`; inspect the match's
contract before changing it. A global replacement cannot distinguish a hint from a stored ID,
wire version, path, or Git branch.
