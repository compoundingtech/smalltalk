{
  description = "st2 - harness-agnostic runner: reconcile a catalog+inbox folder of agent specs, keep their ptys running, deliver messages by moving files";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix.url = "github:nix-community/fenix";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
    # Packaged PTY dependency: the merged revision with atomic metadata patching and the
    # fleet-observation guarantees required by st2 reconciliation.
    pty.url = "github:compoundingtech/pty/504ac7332895fe1fa3767b530dcd99f091f56cda";
    pty.inputs.nixpkgs.follows = "nixpkgs";
    # Shared tooling packages from overengineering: provides the `otelite`
    # OTLP collector binary that `checks.release-integration` drives to prove
    # real span export end-to-end. Pinned to a full rev (like `pty`)
    # so CI is reproducible; bump deliberately via `nix flake lock`.
    effect-utils.url =
      "github:overengineeringstudio/effect-utils/911e2ce0f4ac39d2b54f9ebd6df035234982f721";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
      pty,
      effect-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        providerRustToolchain = fenix.packages.${system}.combine [
          fenix.packages.${system}.stable.cargo
          fenix.packages.${system}.stable.rustc
          fenix.packages.${system}.targets.wasm32-unknown-unknown.stable.rust-std
        ];
        providerRustPlatform = pkgs.makeRustPlatform {
          cargo = providerRustToolchain;
          rustc = providerRustToolchain;
        };

        # Cargo.toml is the single source of truth for the version, so a release
        # bump needs no matching edit here.
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

        # NixStamp for the shared build-versioning contract: the flake rev is a
        # pure input, so baking it lets a hermetic build know its own identity
        # without an impure `.git` read. Same env var + JSON shape as the rest of
        # the fleet (TS `@overeng/utils/node/cli-version`; the otel-scrape Rust
        # reader) — `src/version.rs` reads it via `option_env!("CLI_BUILD_STAMP")`.
        # `self.shortRev`/`lastModified` are absent only for a dirty tree, where
        # `dirtyShortRev` and the working-tree mtime stand in and `dirty` is true.
        sourceRev = self.shortRev or self.dirtyShortRev or "unknown";
        sourceCommitUnix = self.lastModified or 0;
        sourceDirty = !(self ? rev);
        agentSpecRevision =
          if self ? rev then
            self.rev
          else
            "nix-dirty.${self.dirtyRev or self.dirtyShortRev or "unknown"}";
        buildStamp = builtins.toJSON {
          type = "nix";
          inherit version;
          rev = sourceRev;
          commitTs = sourceCommitUnix;
          dirty = sourceDirty;
        };

        completionShells = [
          "bash"
          "zsh"
          "fish"
        ];

        # buildRustPackage compiles the workspace once per derivation, so a gate that differs from
        # an existing derivation only by test selection is folded into that derivation's check
        # phase instead of paying for a second compile. These extra runs deliberately mirror
        # `cargoCheckHook`: same source, profile, offline mode and target dir, so they reuse the
        # artifacts it just built.
        rustHostTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;
        extraCargoTest =
          { label, flags }:
          ''
            echo "--- cargo test: ${label}"
            cargo test -j "$NIX_BUILD_CORES" --release \
              --target ${rustHostTarget} --offline ${pkgs.lib.escapeShellArgs flags}
          '';

        st2 = pkgs.rustPlatform.buildRustPackage {
          pname = "st2";
          inherit version;
          src = self;

          # No git or crates.io-yanked deps in the lockfile, so the lockfile
          # alone pins every input reproducibly — no per-dep outputHashes, and
          # nothing here to hand-patch when a dep bumps.
          cargoLock.lockFile = ./Cargo.lock;

          # This NixStamp is the binary's authoritative build identity; it wins
          # over the LocalStamp `build.rs` bakes from git (which is empty here
          # anyway — a flake source carries no `.git`). Reaches rustc as a plain
          # env var, captured at compile time by `option_env!` (see
          # src/version.rs). A derivation env var change rebuilds the crate.
          CLI_BUILD_STAMP = buildStamp;
          ST2_EXECUTOR_BUILD_IDENTITY = buildStamp;
          AGENT_SPEC_REVISION = agentSpecRevision;

          # The hook integration test executes the shipped Bash scripts with
          # their real jq dependency. `git` is present for tests that initialize
          # throwaway repositories; `installShellFiles` provides
          # `installShellCompletion`.
          nativeBuildInputs = [
            pkgs.bash
            pkgs.git
            pkgs.installShellFiles
            pkgs.jq
          ];

          # Completions are generated by the binary we just built (never
          # committed), so they cannot drift from the actual command tree —
          # `checks.completions` gates that.
          postInstall = ''
            ${pkgs.lib.concatMapStringsSep "\n" (shell: ''
              $out/bin/st2 completions ${shell} > completions-${shell}
            '') completionShells}

            installShellCompletion --cmd st2 \
              --bash completions-bash \
              --zsh completions-zsh \
              --fish completions-fish
          '';

          # Every hermetic test target runs here: the unit tests, agent-spec's discovery test, the
          # real lifecycle-hook integration tests, the reconcile/execute suite, and the
          # parser/CLI/doc-ledger targets that need nothing but `tempfile` and the binary this
          # build just produced. A target belongs in this list iff it is hermetic — an ungated
          # hermetic target is a test that cannot fail CI, which is how `tests/agent_publish.rs`
          # stayed red on `main` unnoticed.
          # The remaining root integration tests assume facilities the Nix build sandbox
          # deliberately lacks: `/usr/bin/git` on a hardcoded `PATH`, live PTY backends, or a
          # systemd `--user` manager. They remain native gates, while the flake proves that its
          # parser and packaged hooks execute. The four sibling derivations below gate the targets
          # that need a different profile, feature set, or `nativeCheckInputs`.
          # `run` is hermetic despite living alongside them — it drives `reconcile`
          # and `execute` against `FakeRunner` and `tempfile` only — so it is gated
          # here. It covers the restart cap's supervision behaviour, which is
          # otherwise unprotected: deleting the flapping cap's per-pass hook left
          # this build green before `run` was added.
          # `driver_expansion` is hermetic for the same reason — it discovers from
          # `tempfile` roots and compares expanded KDL — and it is the only gate on
          # the exact argv every typed harness driver produces. That argv is the
          # whole launch contract, so a silent change to it is the class of defect
          # this build should not ship.
          # `agent_publish` carries nine `#[ignore]`d cases, quarantined on
          # https://github.com/compoundingtech/st2/issues/498: their single-agent fixtures are
          # refused by the root-count rule. Its other 15 cases — CAS staleness, control-directory
          # swap, ownership markers — gate here.
          # `--workspace` because the root is a real package: without it cargo
          # selects only `st2` and silently skips the `agent-spec` crate.
          cargoTestFlags = [
            "--workspace"
            "--exclude"
            "st2-resource-providers"
            "--exclude"
            "st2-github-issue-component"
            "--exclude"
            "st2-github-pr-component"
            "--exclude"
            "st2-pty-stats-component"
            "--exclude"
            "st2-vista-component"
            "--lib"
            "--bins"
            "--test"
            "discovery"
            "--test"
            "codex_hooks"
            "--test"
            "hooks"
            "--test"
            "run"
            "--test"
            "driver_expansion"
            "--test"
            "agent_address"
            "--test"
            "agent_desired_state"
            "--test"
            "claude_hooks"
            "--test"
            "agent_publish"
            "--test"
            "catalog_graph"
            "--test"
            "invariants"
            "--test"
            "message"
            "--test"
            "status_agents"
            "--test"
            "validate"
            "--test"
            "vrs_ledger"
            # Lifecycle tests fork while holding temporary sockets and executables.
            # Serial execution prevents sibling tests from inheriting those live handles.
            "--"
            "--test-threads=1"
          ];

          # A few unit tests write under $HOME; the sandbox HOME is not writable.
          preCheck = "export HOME=$(mktemp -d)";

          meta = {
            description = "Harness-agnostic runner over a unified catalog+inbox folder of agent specs";
            homepage = "https://github.com/compoundingtech/st2";
            license = pkgs.lib.licenses.mit;
            mainProgram = "st2";
          };
        };

        # Production variant for catalogs that declare wasm resource-profile resolvers. Keep the
        # default package lightweight; consumers opt into the wasmtime closure explicitly.
        #
        # Doubles as `checks.wasm-resolver-feature`: the default hermetic suite runs with the
        # production feature set, and the feature-gated targets reuse that same build.
        st2WasmResolver = st2.overrideAttrs (old: {
          pname = "st2-wasm-resolver";
          cargoBuildFeatures = (old.cargoBuildFeatures or [ ]) ++ [ "wasm-resolver" ];
          cargoCheckFeatures = (old.cargoCheckFeatures or [ ]) ++ [ "wasm-resolver" ];
          # Wasmtime's Cranelift build and the feature-gated resolver tests need the Rust toolchain
          # inherited from buildRustPackage plus an LLVM linker on every supported platform.
          nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.lld ];
          # Non-vacuous feature gate: both the runner's live resync integration and agent-spec's
          # wasm ABI/containment suite execute with the same features as the production variant.
          postCheck = extraCargoTest {
            label = "wasm-resolver feature suite";
            flags = [
              "--features"
              "wasm-resolver"
              "--workspace"
              "--exclude"
              "st2-resource-providers"
              "--exclude"
              "st2-github-issue-component"
              "--exclude"
              "st2-github-pr-component"
              "--exclude"
              "st2-pty-stats-component"
              "--exclude"
              "st2-vista-component"
              "--test"
              "resync"
              "--test"
              "resync_notify_chain"
              "--test"
              "profile_wasm"
            ];
          };
        });

        providerComponentPackages = {
          "st2-github-issue-component" = "st2_github_issue_component";
          "st2-github-pr-component" = "st2_github_pr_component";
          "st2-pty-stats-component" = "st2_pty_stats_component";
          "st2-vista-component" = "st2_vista_component";
        };

        # One cargo invocation builds all four guest crates: they share the same wasm32 dependency
        # graph, so a derivation per component compiled it four times. Install paths are unchanged
        # and every component package attr points at this single output.
        st2ProviderComponents = providerRustPlatform.buildRustPackage {
          pname = "st2-provider-components";
          inherit version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          buildPhase = ''
            runHook preBuild
            cargo build --offline --release --target wasm32-unknown-unknown \
              ${
                pkgs.lib.concatMapStringsSep " " (package: "-p ${package}") (
                  pkgs.lib.attrNames providerComponentPackages
                )
              }
            runHook postBuild
          '';
          doCheck = false;
          nativeBuildInputs = [
            pkgs.lld
            pkgs.wasm-tools
          ];
          installPhase = ''
            runHook preInstall
            mkdir -p "$out/share/st2/providers"
            ${pkgs.lib.concatMapStringsSep "\n" (wasmName: ''
              wasm-tools component new \
                "target/wasm32-unknown-unknown/release/${wasmName}.wasm" \
                -o "$out/share/st2/providers/${wasmName}.component.wasm"
            '') (pkgs.lib.attrValues providerComponentPackages)}
            runHook postInstall
          '';
        };

        providerComponentPath =
          wasmName: "${st2ProviderComponents}/share/st2/providers/${wasmName}.component.wasm";

        # Production variant for catalogs whose resource profiles are WASIp2 components.
        #
        # Doubles as `checks.wasip2-resource-providers`: one compile of the runtime feature serves
        # the provider/supervisor end-to-end targets and the Component Model executor's fixture and
        # cache trust boundary. The default workspace remains covered by `checks.st2`.
        st2ProviderRuntime = st2.overrideAttrs (old: {
          pname = "st2-provider-runtime";
          cargoBuildFeatures = (old.cargoBuildFeatures or [ ]) ++ [ "wasip2-provider-runtime" ];
          cargoCheckFeatures = [ ];
          nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
            pkgs.lld
            pty.packages.${system}.default
          ];
          ST2_GITHUB_ISSUE_COMPONENT = providerComponentPath "st2_github_issue_component";
          ST2_GITHUB_PR_COMPONENT = providerComponentPath "st2_github_pr_component";
          ST2_PTY_STATS_COMPONENT = providerComponentPath "st2_pty_stats_component";
          ST2_VISTA_COMPONENT = providerComponentPath "st2_vista_component";
          cargoTestFlags = [
            "-p"
            "st2-resource-providers"
            "--lib"
            "--test"
            "github_issue_component"
            "--test"
            "github_pr_component"
          ];
          postCheck =
            extraCargoTest {
              label = "wasip2 supervisor integration";
              flags = [
                "-p"
                "st2"
                "--features"
                "wasip2-provider-runtime"
                "--test"
                "resource_profile_supervisor_e2e"
                "--test"
                "resource_provider_e2e"
              ];
            }
            + extraCargoTest {
              label = "wasip2 resource executor";
              flags = [
                "-p"
                "st2-resource-wasip2"
                "--features"
                "runtime"
                "--lib"
                "--test"
                "executor"
              ];
            };
        });

        # Sandbox-safe integration episodes the package's own release-mode boundary cannot reach,
        # sharing one default-feature build because they differ only by test selection:
        #   * `atomic_pty_snapshot` — the atomic snapshot boundary, split out of the broad doctor
        #     suite (some doctor cases need facilities the sandbox lacks). A target holding exactly
        #     one test makes the gate structurally non-vacuous: a missing target is a cargo error,
        #     never a zero-match pass.
        #   * `parked_recovery` — the parked-task recovery episode against real processes. Both
        #     single-pass entry points build a fresh `FlappingCap`, so `up --once` can never park
        #     anything and the package's boundary would never reach this path.
        #   * `otel_export` — OTLP span export. The test skips unless `ST2_OTELITE_BIN` points at a
        #     real collector; pinning `otelite` here and leaving `ST2_ALLOW_OTEL_SKIP` unset is what
        #     makes a broken export path fail instead of silently skipping.
        st2ReleaseIntegration = st2.overrideAttrs (old: {
          pname = "st2-release-integration-check";
          # The supervisor snapshots pty sessions on every pass even for an exec-only catalog, and
          # both `st2 up --once` drivers shell out to `pty list --json`, so the real producer must
          # be on PATH — without it no pass reconciles and no task ever reaches the park.
          nativeCheckInputs = (old.nativeCheckInputs or [ ]) ++ [
            pty.packages.${system}.default
            effect-utils.packages.${system}.otelite
          ];
          ST2_OTELITE_BIN = "${effect-utils.packages.${system}.otelite}/bin/otelite";
          cargoTestFlags = [
            "--test"
            "atomic_pty_snapshot"
            "--test"
            "parked_recovery"
            "--test"
            "otel_export"
          ];
        });

        # Bootstrap's crash/race tests and the message CLI's crash/recovery controls are both
        # compiled only with debug assertions, so they share one derivation. Keep the package's
        # release-mode test boundary unchanged. `bootstrap_` is a positional name filter, so it
        # needs its own invocation — in a shared one it would also filter `message_cli` to nothing.
        st2DebugAssertions = st2.overrideAttrs (_: {
          pname = "st2-debug-assertions-check";
          CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS = "true";
          cargoTestFlags = [
            "--test"
            "message_cli"
          ];
          postCheck = extraCargoTest {
            label = "catalog bootstrap transactions";
            flags = [
              "--test"
              "catalog_apply"
              "bootstrap_"
            ];
          };
        });

        hookSuccessorSource = pkgs.runCommand "st2-hook-successor-source" { } ''
          cp -R ${self} $out
          chmod -R u+w $out
          printf '\n# Nix hook replacement acceptance probe.\n' >> $out/hooks/codex-stop.sh
        '';

        st2HookSuccessor = st2.overrideAttrs (_: {
          pname = "st2-hook-successor";
          src = hookSuccessorSource;
          # This build exists only to supply a second binary with different embedded hook bytes to
          # `checks.hooks-replacement`. Its test suite is `checks.st2`'s, on source that differs
          # only by an appended hook comment, so running it again buys nothing.
          doCheck = false;
          CLI_BUILD_STAMP = builtins.toJSON {
            type = "nix";
            inherit version;
            rev = "hook-successor";
            commitTs = sourceCommitUnix;
            dirty = false;
          };
        });
      in
      {
        packages.st2 = st2;
        packages.st2-wasm-resolver = st2WasmResolver;
        packages.st2-provider-runtime = st2ProviderRuntime;
        # All four components come out of one build; the install paths are unchanged.
        packages.st2-github-issue-component = st2ProviderComponents;
        packages.st2-github-pr-component = st2ProviderComponents;
        packages.st2-pty-stats-component = st2ProviderComponents;
        packages.st2-vista-component = st2ProviderComponents;
        packages.default = st2;

        # `nix flake check` is the whole CI: it builds the package — which runs
        # the hermetic portion of the in-tree `cargo test` suite via doCheck —
        # and evaluates the `--help` + completions smoke tests below.
        #
        # `cargo fmt --check` / `clippy -D warnings` are intentionally NOT gated:
        # this is a packaging PR on an actively-developed, hand-crafted tree, and a
        # repo-wide formatting/lint gate here would fight the maintainer's own
        # commits on every rebase. The devShell ships rustfmt + clippy for whoever
        # wants them.
        checks.st2 = st2;
        checks.release-integration = st2ReleaseIntegration;
        checks.debug-assertions = st2DebugAssertions;
        checks.wasm-resolver-feature = st2WasmResolver;
        checks.wasip2-resource-providers = st2ProviderRuntime;
        checks.provider-components = st2ProviderComponents;
        # Exercise the shipped binary, not a cargo-side surrogate: its version entrypoint runs and
        # the same artifact strictly admits a catalog carrying a real wasm profile module.
        checks.wasm-resolver-artifact = pkgs.runCommand "st2-wasm-resolver-artifact-${version}" { } ''
          ${st2WasmResolver}/bin/st2 --version |
            ${pkgs.gnugrep}/bin/grep -E '^st2 [^[:space:]]+' >/dev/null

          catalog="$TMPDIR/catalog"
          mkdir -p "$catalog/resolvers" "$catalog/h/worker"
          cp ${self}/crates/agent-spec/tests/fixtures/demo_resolver.wasm \
            "$catalog/resolvers/goal.wasm"
          cat > "$catalog/catalog.kdl" <<'EOF'
          profile "dev.schickling.agent-goal" {
            wasm "resolvers/goal.wasm"
            class "immediate"
          }
          EOF
          cat > "$catalog/h/worker/agent.kdl" <<'EOF'
          agent "worker" {
            host "h"
            command "true"
            resource "goal" uri="dev.schickling.agent-goal://h/worker" reason="Mission."
          }
          EOF

          ${st2WasmResolver}/bin/st2 validate "$catalog" --host h --strict
          touch "$out"
        '';

        # Real producer-consumer contract: st2 consumes `pty list --json` from the exact pty
        # revision that owns fleet observation. Fake CLI fixtures below still cover malformed
        # output and a wedged child; this check proves the healthy 0/75/100/500-session path crosses
        # both packaged binaries within st2's short outer deadline.
        # The pi extension's only compile-time coupling to pi.
        #
        # `hooks/pi-channel.ts` is shipped as an opaque asset inside the content-addressed hook set
        # and pi loads the TypeScript directly, so its `import type` is erased and nothing in a
        # cargo build ever reads it. This check is what makes that import real: it type-checks the
        # asset against the exact pi release st2 was written for.
        #
        # Measured over pi 0.74.0..0.84.2 (41 releases): the whole `types.d.ts` changed in 17 of 40
        # transitions, but the surface this extension depends on changed in exactly ONE, additively.
        # So the check is close to noise-free, and it has teeth on the failure that matters most —
        # using pi's idle proof as a property rather than calling it type-errors, and that mistake
        # would otherwise silently turn every mid-turn delivery into a plain send.
        #
        # Pinned as tarballs rather than an npm lockfile because pi bundles its sibling packages
        # without integrity hashes, which `fetchNpmDeps` cannot express.
        checks.pi-extension-types =
          let
            piVersion = "0.84.2";
            piTarball = pkgs.fetchurl {
              url = "https://registry.npmjs.org/@earendil-works/pi-coding-agent/-/pi-coding-agent-${piVersion}.tgz";
              hash = "sha256-lbiZzXsaDB8BdMe/M6tCdDXjVTp9H0dWZhqpx/Gmj/o=";
            };
            nodeTypesTarball = pkgs.fetchurl {
              url = "https://registry.npmjs.org/@types/node/-/node-26.2.0.tgz";
              hash = "sha256-ATysqeRVcLEeqPuz+LnjJ0NpNrNiiAZAtZ+f4qz93sk=";
            };
          in
          pkgs.runCommand "st2-pi-extension-types-${version}" {
            nativeBuildInputs = [
              pkgs.gnutar
              pkgs.nodejs
              pkgs.typescript
            ];
          } ''
            cp -R ${self}/hooks hooks
            chmod -R u+w hooks

            modules=hooks/typecheck/node_modules
            mkdir -p "$modules/@earendil-works/pi-coding-agent" "$modules/@types/node"
            tar -xzf ${piTarball} -C "$modules/@earendil-works/pi-coding-agent" --strip-components=1
            tar -xzf ${nodeTypesTarball} -C "$modules/@types/node" --strip-components=1

            # Non-vacuous: the asset must exist and must actually import pi's types, or a green
            # result here would mean nothing.
            test -f hooks/pi-channel.ts
            grep -q '@earendil-works/pi-coding-agent' hooks/pi-channel.ts
            test -f hooks/omp-channel.ts
            grep -q '@earendil-works/pi-coding-agent' hooks/omp-channel.ts

            tsc --noEmit -p hooks/typecheck/tsconfig.json

            # Runtime smoke: the type gate is provably blind to execution-order defects (a TDZ
            # use-before-declaration shipped green through it), so the asset is transpiled and
            # actually driven through its open path.
            #
            # Each handler is driven with three contexts — bare, fully populated, and one whose
            # every telemetry pull throws — because a bare context takes the fail-open branch and
            # never executes the harness-context producer's body at all, which is the same blind
            # spot in a new place. The channel is a recorder rather than `true`, so the smoke reads
            # the frames back and asserts the wire `src/pi_channel.rs` decodes: with a pipe nobody
            # reads, a producer that silently emits nothing is indistinguishable from a working
            # one, and that failure looks exactly like the pre-producer state where every
            # declaration's context reads null. Nothing else couples the two halves of that wire —
            # they are different languages in different files.
            ${pkgs.esbuild}/bin/esbuild hooks/pi-channel.ts \
              --format=esm --platform=node --target=es2022 \
              --outfile=hooks/typecheck/smoke-out/pi-channel.mjs
            ${pkgs.nodejs}/bin/node hooks/typecheck/smoke.mjs
            ${pkgs.esbuild}/bin/esbuild hooks/omp-channel.ts \
              --format=esm --platform=node --target=es2022 \
              --outfile=hooks/typecheck/smoke-out/omp-channel.mjs
            ${pkgs.nodejs}/bin/node hooks/typecheck/omp-smoke.mjs
            touch $out
          '';

        checks.pty-fleet-contract = pkgs.runCommand "st2-pty-fleet-contract-${version}" {
          nativeBuildInputs = [
            pkgs.coreutils
            pkgs.jq
            pkgs.nodejs
            pty.packages.${system}.default
            st2
          ];
        } ''
          export HOME=$(mktemp -d)
          catalog=$(mktemp -d)
          mkdir -p "$catalog/agents/contract/gone"
          printf '%s\n' \
            'agent "gone" { host "contract"; retired #true; command "true" }' \
            > "$catalog/agents/contract/gone/agent.kdl"

          # Run the exact packaged producer's deterministic fault seams. These prove EPERM avoids
          # socket fallback and hundreds of indefinitely-hung ambiguous probes share one deadline.
          test_config=$(mktemp --suffix=.mjs)
          printf '%s\n' 'export default { test: {} }' > "$test_config"
          node \
            ${pty.packages.${system}.default}/lib/pty/node_modules/vitest/vitest.mjs \
            run tests/list-liveness-budget.test.ts \
            --config "$test_config" \
            --root ${pty.packages.${system}.default}/lib/pty

          for fleet_size in 0 75 100 500; do
            root=$(mktemp -d)
            i=0
            while test "$i" -lt "$fleet_size"; do
              session=$(printf 'session-%03d' "$i")
              : > "$root/$session.sock"
              printf '%s\n' "$$" > "$root/$session.pid"
              i=$((i + 1))
            done

            PTY_ROOT="$root" timeout 2s pty list --json > "pty-$fleet_size.json"
            jq -e --argjson size "$fleet_size" \
              'length == $size and all(.status == "running")' \
              "pty-$fleet_size.json" >/dev/null

            PTY_ROOT="$root" timeout 2s \
              st2 doctor --catalog "$catalog" --host contract \
              > "doctor-$fleet_size.out"
            grep -F 'contract.gone retirement complete' \
              "doctor-$fleet_size.out" >/dev/null
          done

          touch $out
        '';

        # Smoke test that the built binary actually runs and its command tree is
        # wired, independent of the in-tree `cargo test`.
        checks.help = pkgs.runCommand "st2-help-${version}" { } ''
          export HOME=$(mktemp -d)
          ${st2}/bin/st2 --help > /dev/null
          ${st2}/bin/st2 ls --help > /dev/null
          touch $out
        '';

        # Guards the completions contract: every shell we install still generates
        # a non-empty script, and fish in particular still binds to `st2` (the
        # name the installed st2.fish file claims). Written to files first —
        # clap_complete streams to stdout and panics on a `grep -q` early
        # pipe-close (BrokenPipe), which the real `> file` usage never hits.
        checks.completions = pkgs.runCommand "st2-completions-${version}" { } ''
          ${pkgs.lib.concatMapStringsSep "\n" (shell: ''
            ${st2}/bin/st2 completions ${shell} > ${shell}.out
            test -s ${shell}.out || { echo "empty ${shell} completions" >&2; exit 1; }
          '') completionShells}

          grep -q 'complete -c st2' fish.out \
            || { echo "fish completions do not bind to \`st2\`" >&2; exit 1; }

          touch $out
        '';

        # End-to-end receipt proof across two real Nix-built binaries. The
        # synthetic successor changes embedded hook bytes while deliberately
        # retaining the same source timestamp: replacement must be explicit,
        # and must not be mislabeled as a downgrade.
        checks.hooks-replacement = pkgs.runCommand "st2-hooks-replacement-${version}" {
          nativeBuildInputs = [
            pkgs.git
            pkgs.jq
          ];
        } ''
          export HOME=$(mktemp -d)
          export ST_HOOKS=$HOME/hooks

          ${st2}/bin/st2 hooks install
          original_dir=$(jq -r '.directory' "$ST_HOOKS/current.json")
          jq -e \
            --arg rev ${pkgs.lib.escapeShellArg sourceRev} \
            --argjson commit ${toString sourceCommitUnix} \
            --argjson dirty ${builtins.toJSON sourceDirty} \
            '.st2GitSha == $rev and .sourceCommitUnix == $commit and .sourceDirty == $dirty' \
            "$ST_HOOKS/current.json" >/dev/null

          if ${st2HookSuccessor}/bin/st2 hooks install 2>replacement.err; then
            echo "same-order hook replacement unexpectedly succeeded" >&2
            exit 1
          fi
          grep -F -- '--replace' replacement.err >/dev/null

          ${st2HookSuccessor}/bin/st2 hooks install --replace
          ${st2HookSuccessor}/bin/st2 hooks verify
          ${st2}/bin/st2 hooks verify-own
          jq -e \
            --argjson commit ${toString sourceCommitUnix} \
            '.st2GitSha == "hook-successor" and .sourceCommitUnix == $commit and .sourceDirty == false' \
            "$ST_HOOKS/current.json" >/dev/null

          # The old binary keeps using its own previously installed immutable
          # set even though the successor is now selected globally.
          mkdir -p "$HOME/catalog/agents/h/worker" "$HOME/workspace"
          git init -q "$HOME/workspace"
          cat > "$HOME/catalog/agents/h/worker/agent.kdl" <<EOF
          agent "worker" {
            host "h"
            workspace "$HOME/workspace"
            command "exec codex"
            render { file "hook-path" "\$ST_HOOKS/codex-stop.sh" }
          }
          EOF
          ${st2}/bin/st2 up "$HOME/catalog" --host h --materialize-only
          grep -Fx \
            "$ST_HOOKS/$original_dir/codex-stop.sh" \
            "$HOME/workspace/hook-path" >/dev/null

          touch $out
        '';

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.git
            # wasm guest modules (resource-profile resolvers) link with lld; nixpkgs rustc does
            # not bundle rust-lld the way the rustup toolchain does.
            pkgs.lld
            pty.packages.${system}.default
            # Local runs of the OTLP export integration gate
            # (`cargo test --test otel_export`) need the same collector the
            # Nix check pins; `ST2_OTELITE_BIN` points at it.
            effect-utils.packages.${system}.otelite
          ];
          # Same collector the Nix gate pins, so a bare
          # `cargo test --test otel_export` in this shell runs against it.
          ST2_OTELITE_BIN = "${effect-utils.packages.${system}.otelite}/bin/otelite";
        };
      }
    );
}
