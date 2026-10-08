{ pkgs, craneLib, st3, st3Check }:
let
  manifest = pkgs.writeText "Cargo.toml" ''
    [package]
    name = "cache-probe"
    version = "0.1.0"
    edition = "2024"
    [dependencies]
    itoa = "=1.0.18"
    anyhow = "=1.0.104"
    [features]
    extra = []
    test-support = []
    [[bin]]
    name = "cache-probe"
    path = "src/main.rs"
    [[bin]]
    name = "unselected"
    path = "src/unselected.rs"
  '';
  lockContents = ''
    version = 4
    [[package]]
    name = "cache-probe"
    version = "0.1.0"
    dependencies = ["itoa", "anyhow"]
    [[package]]
    name = "anyhow"
    version = "1.0.104"
    source = "registry+https://github.com/rust-lang/crates.io-index"
    checksum = "330a5ed07fa54e4702c9d6c4174f74427fc0ef6e214bbd677ae50a5099946470"
    [[package]]
    name = "itoa"
    version = "1.0.18"
    source = "registry+https://github.com/rust-lang/crates.io-index"
    checksum = "8f42a60cbdf9a97f5d2305f08a87dc4e09308d1276d28c869c684d7777685682"
  '';
  lock = pkgs.writeText "Cargo.lock" lockContents;
  source =
    message: lockFile:
    pkgs.runCommand "source"
      {
        inherit manifest lockFile;
        buildScript = pkgs.writeText "build.rs" (builtins.readFile ../crates/st3/build.rs);
        code = pkgs.writeText "main.rs" ''
          fn main() {
            println!("${message}:{}:{}:{}", itoa::Buffer::new().format(42),
              option_env!("CLI_BUILD_STAMP").unwrap_or("missing"), cfg!(feature = "extra"));
          }
        '';
      }
      ''
        mkdir -p "$out/src" "$out/tests"
        cp "$manifest" "$out/Cargo.toml"
        cp "$lockFile" "$out/Cargo.lock"
        cp "$code" "$out/src/main.rs"
        cp "$buildScript" "$out/build.rs"
        echo 'fn main() {}' > "$out/src/unselected.rs"
        touch "$out/tests/smoke.rs"
      '';
  makePackage =
    message: stamp: lockFile: overrides:
    let
      src = source message lockFile;
      cacheCargo = import ./cargo-artifacts.nix {
        inherit pkgs craneLib;
        self = src;
      };
    in
    cacheCargo (
      pkgs.rustPlatform.buildRustPackage (
        {
          pname = "cache-probe";
          version = "0.1.0";
          inherit src;
          cargoLock.lockFile = lockFile;
          cargoBuildFlags = [
            "--verbose"
            "--bin"
            "cache-probe"
          ];
          cargoTestFlags = [
            "--test"
            "smoke"
          ];
          CLI_BUILD_STAMP = stamp;
        }
        // overrides
      )
    );
  first = makePackage "first" "revision-one" lock { };
  changed = makePackage "changed" "revision-two" lock { };
  codeOnly = makePackage "changed" "revision-one" lock { };
  changedCheck = makePackage "first" "revision-one" lock {
    checkPhase = "echo a different runtime check";
  };
  feature = makePackage "changed" "revision-two" lock { buildFeatures = [ "extra" ]; };
  debug = makePackage "first" "revision-one" lock {
    CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS = "true";
  };
  compilerFlags = makePackage "first" "revision-one" lock {
    RUSTFLAGS = "-C opt-level=1";
  };
  changedLock = makePackage "first" "revision-one" (pkgs.writeText "Cargo.lock" (
    lockContents + "\n# different lock input\n"
  )) { };
  artifacts = package: package.cargoDependencyArtifacts.drvPath;
  production = first.overrideAttrs (old: {
    outputs = [
      "out"
      "cargoBuildArtifacts"
    ];
    postBuild = (old.postBuild or "") + ''
      prepareAndInstallCargoArtifactsDir "$cargoBuildArtifacts"
    '';
  });
  fromProduction = changed.overrideAttrs (_: {
    cargoArtifacts = production.cargoBuildArtifacts;
  });
  productionChecks = first.overrideAttrs (_: {
    cargoArtifacts = production.cargoBuildArtifacts;
    buildPhase = ''
      # Check phases add tools to PATH; production does not capture that PATH.
      export PATH="${pkgs.hello}/bin:$PATH"
      cargoBuildHook > compilation.log 2>&1
      cat compilation.log
      grep -F 'Fresh cache-probe v' compilation.log
    '';
  });
  verifyFresh =
    package:
    package.overrideAttrs (_: {
      buildPhase = ''
        cargoBuildHook > compilation.log 2>&1
        cat compilation.log
          grep -F 'Fresh anyhow v' compilation.log
          grep -F 'Fresh itoa v' compilation.log
      '';
    });
in
# The dummy-only dependency exception must never remove shipped-hook assertions.
assert builtins.all (package:
  pkgs.lib.hasInfix "Missing embedded Claude hook:" package.postPatch
  && pkgs.lib.hasInfix "Wrong embedded Claude hook interpreter" package.postPatch
  && pkgs.lib.hasInfix (builtins.unsafeDiscardStringContext
    ''[ "$shebang" != "#!${pkgs.bash}/bin/bash" ]'') package.postPatch
) [ st3 st3Check ];
assert artifacts first == artifacts changed;
assert artifacts first == artifacts codeOnly;
assert artifacts first == artifacts changedCheck;
assert artifacts first != artifacts feature;
assert artifacts first != artifacts debug;
assert artifacts first != artifacts compilerFlags;
assert artifacts first != artifacts changedLock;
pkgs.runCommand "cargo-artifact-reuse" { } ''
  test "$(${verifyFresh first}/bin/cache-probe)" = 'first:42:revision-one:false'
  test "$(ls ${verifyFresh first}/bin)" = 'cache-probe'
  test "$(${verifyFresh changed}/bin/cache-probe)" = 'changed:42:revision-two:false'
  test "$(${verifyFresh codeOnly}/bin/cache-probe)" = 'changed:42:revision-one:false'
  test "$(${feature}/bin/cache-probe)" = 'changed:42:revision-two:true'
  test "$(${verifyFresh fromProduction}/bin/cache-probe)" = 'changed:42:revision-two:false'
  test "$(${productionChecks}/bin/cache-probe)" = 'first:42:revision-one:false'
  touch "$out"
''
