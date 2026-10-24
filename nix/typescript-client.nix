# The `typescript-client` required check as a sandboxed derivation.
#
# `scripts/ci-typescript-client` runs over the root workspace's frozen installation. Nix has no network
# in a build, so the installation is its own fixed-output derivation: `pnpm install --frozen-lockfile`
# over only the manifests and the lockfile, keeping the `node_modules` trees it produces (the workspace
# uses the hoisted linker, so they are plain directories with relative links). The check restores them
# over the source and runs the same script, unchanged.
#
# Refreshing the hash: the installation name carries the lockfile hash and effect-utils source identity.
# `nix/typescript-client-deps.hash` records both the output hash and the effect-utils narHash it was measured
# with, so consumers overriding that input fail before building. `scripts/nix-typescript-client-deps-hash`
# refreshes both values. pnpm-workspace.yaml installs the same supported architectures on every host.
{ effect-utils, system, self, version }:
let
  pkgs = import effect-utils.inputs.nixpkgs { inherit system; };
  lib = pkgs.lib;
  # pnpm and Node 24 from effect-utils' own nixpkgs, as the web shell (`nix/web-shell.nix`) has them.
  pnpm = effect-utils.lib.mkPnpm { inherit pkgs; };
  node = pkgs.nodejs_24;
  # A fixed-output path is named by its name and hash, not by its builder inputs. The lockfile records the staged
  # `@overeng/*` sources by path only, so both the lockfile and the effect-utils revision go into the name: a bump of
  # either builds a new installation instead of reusing a stale one from a store or cache.
  lockHash = builtins.substring 0 12 (builtins.hashFile "sha256" ../pnpm-lock.yaml);
  sourcesHash = builtins.substring 0 12 (builtins.hashString "sha256" effect-utils.narHash);
  dependencyPin = builtins.fromJSON (builtins.readFile ./typescript-client-deps.hash);
  manifests = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../package.json
      ../pnpm-lock.yaml
      ../pnpm-workspace.yaml
      ../pnpm-source-inputs.json
      ../apps/fractal-web/package.json
      ../packages/fractal-sdk/package.json
      ../apps/ios/package.json
      ../clients/typescript/fractal-client/package.json
      ../clients/typescript/fractal-scenarios/package.json
      ../clients/typescript/fractal-views/package.json
      ../packages/fractal-ui/package.json
    ];
  };
  installation = pkgs.runCommand "typescript-client-installation-${lockHash}-${sourcesHash}" {
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash =
      if effect-utils.narHash == dependencyPin.effectUtilsNarHash then dependencyPin.hash
      else throw "fractal's effect-utils input is overridden or changed (got ${effect-utils.narHash}, hash measured with ${dependencyPin.effectUtilsNarHash}); consumers must resolve effect-utils to fractal's pin, or run scripts/nix-typescript-client-deps-hash";
    passthru.effectUtilsNarHash = effect-utils.narHash;
    nativeBuildInputs = [ pnpm node pkgs.cacert ];
    SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
  } ''
    export HOME=$TMPDIR/home
    mkdir -p "$HOME" source
    cp -r ${manifests}/. source
    chmod -R u+w source
    cd source
    # The workspace's `@overeng/*` packages are `file:` sources staged from effect-utils, as the web shell does.
    mkdir repos
    ln -s ${effect-utils} repos/effect-utils
    node -e '
      const { readFileSync } = require("node:fs");
      const { spawnSync } = require("node:child_process");
      const paths = JSON.parse(readFileSync("pnpm-source-inputs.json", "utf8")).sourceInputPaths;
      const result = spawnSync(process.execPath, [
        process.argv[1], "publish", process.cwd(), ".devenv/pnpm-source-inputs", ...paths,
      ], { stdio: "inherit" });
      process.exit(result.status ?? 1);
    ' ${effect-utils}/nix/devenv-modules/tasks/shared/stage-pnpm-source-inputs.mjs
    pnpm install --frozen-lockfile --store-dir "$TMPDIR/store"
    # Both name the install directory and the time; neither is needed to run the workspace.
    find . -name node_modules -prune -exec rm -f '{}/.modules.yaml' '{}/.pnpm-workspace-state-v1.json' ';'
    # pacquet can leave atomic-write staging files with PID/time/counter names.
    # They are not package inputs and make otherwise identical installations nondeterministic.
    find . -type f -name 'package.json_pacquet-stage_*' -delete
    mkdir "$out"
    find . -name node_modules -prune -print | while IFS= read -r directory; do
      mkdir -p "$out/$(dirname "$directory")"
      cp -a "$directory" "$out/$directory"
    done
  '';
in
pkgs.runCommand "typescript-client-${version}" {
  nativeBuildInputs = [ pnpm node pkgs.bubblewrap pkgs.coreutils pkgs.bash ];
  passthru = { inherit installation; };
} ''
  export HOME=$(mktemp -d)
  cp -r ${self} source
  chmod -R u+w source
  cd source
  cp -a ${installation}/. .
  chmod -R u+w .
  # The installed tools start with `#!/usr/bin/env node`, which the build sandbox has no path for:
  # run in a fresh root that has one (network off, as the build already is).
  bwrap --bind /nix /nix --bind "$PWD" "$PWD" --bind "$HOME" "$HOME" --ro-bind /etc /etc \
    --dev /dev --proc /proc --tmpfs /tmp \
    --dir /usr/bin --ro-bind ${pkgs.coreutils}/bin/env /usr/bin/env \
    --dir /bin --ro-bind ${pkgs.bash}/bin/sh /bin/sh \
    --unsetenv PYTHONPATH --unshare-net --chdir "$PWD" -- bash scripts/ci-typescript-client
  touch "$out"
''
