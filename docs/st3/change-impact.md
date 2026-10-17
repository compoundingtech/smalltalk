# Risky classes: what always runs in full

The module graph (`docs/st3/module-graph.md`) lets a combined tree retest only what both sides could
affect. Some changes cannot be bounded by a graph, because what they break is in data, wire formats,
credentials or the build rather than in a call: those always run the full test set of their class.
The classes and the rule that selects them are in `.st/risky-classes.json`, and
`scripts/classify-risk BASE HEAD` applies them to `git diff BASE...HEAD`.

A class matches on **paths** (globs, `**` crosses directories) and, where the path is too broad to
be risky as a whole, on **content**: an added or removed diff line that matches a pattern in files
under the class's globs. Content patterns exist because the store's schema lives in
`crates/st3-store/src/store.rs` and `store/`, which are not risky files; an added index or trigger there
is a schema change (release notes already treat it as one).

| class | selects | forces |
|---|---|---|
| schema | schema version and migration files, `st3-schema`, `st-storage-version`, `st3-migrate`, checkpoint rules, `docs/st3/schema.md`; DDL (`CREATE`/`ALTER`/`DROP` table, index, trigger, view; `user_version`) in store and smallclaims sources | integration, store and smallclaims suites, a release-notes upgrade fragment |
| protocol | wire and resource-protocol crates, the client crate and its generator, `clients/`, `wit/`, `api.rs` and `api/`, `peer`, `relay_trace`, client-v0 and fabric protocol docs | integration, client contract tests, codegen `--check`, `scripts/read-contract-report` |
| generated-contracts | the generator, generated clients, `action-coverage.json`, the read-contract report; contract hashes and registry digests in content | codegen `--check`, action coverage, client contract tests |
| authorization | client-v0 and web API files, `sekrets`, accounts, roster admission and delegation, device signing, `.st/**`; trusted keys, pairing, signing keys in content | integration, authorization tests, `scripts/audit-claim-signatures-test` |
| claim-folding-and-replay | card fold, the IVM files of the store, checkpoint files, projection, smallclaims replay and IVM | integration, purity and convergence suites |
| config-and-build | every `Cargo.toml`, `Cargo.lock`, `.cargo/`, `flake.nix`, `flake.lock`, `nix/`, `.config/nextest.toml`, `.github/`, `scripts/ci-*`, `build.rs`, `config.rs` | every required check in full |
| routing-and-delivery | mailbox, placement, suspension, rollout, reconcile, mailbox store files; route registration in `api.rs` | integration, delivery and messaging fault suites |
| release-notes | `release-notes/` | `scripts/check-release-impact` |

The list is a floor, not a ceiling. Add a class or a pattern when a break gets past it, with the
change that broke; remove nothing without evidence that the class never fired usefully.
