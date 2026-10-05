{
  pkgs,
  self,
  craneLib,
  extraDummyScript ? "",
}:

# Keep Nixpkgs' Cargo phases, vendoring, toolchain and linker flags identical in
# the dependency and application builds. Crane supplies source stubbing and
# artifact transport; the application still receives the entire flake source.
package:
let
  lib = pkgs.lib;
  cargoFlags =
    flags:
    if flags == [ ] || builtins.head flags == "--" then
      [ ]
    else
      [ (builtins.head flags) ] ++ cargoFlags (builtins.tail flags);
  # Crane stubs explicitly declared tests; Cargo also discovers tests/foo.rs
  # and tests/foo/main.rs. Preserve those target names without depending on
  # their contents, so explicit --test selectors also work in the dummy build.
  originalSource = if self._isLibCleanSourceWith or false then self.origSrc else self;
  sourcePrefix = builtins.unsafeDiscardStringContext (toString originalSource + "/");
  autoTests = lib.concatMapStrings (
    cargoToml:
    let
      manifest = builtins.fromTOML (builtins.readFile cargoToml);
      testDir = builtins.dirOf cargoToml + "/tests";
      relativeDir = builtins.dirOf (lib.removePrefix sourcePrefix cargoToml);
      entries =
        if manifest ? package && (manifest.package.autotests or true) && builtins.pathExists testDir then
          builtins.readDir testDir
        else
          { };
    in
    lib.concatMapStrings (
      name:
      let
        testPath =
          if entries.${name} == "regular" && lib.hasSuffix ".rs" name then
            "tests/${name}"
          else if entries.${name} == "directory" && builtins.pathExists (testDir + "/${name}/main.rs") then
            "tests/${name}/main.rs"
          else
            null;
        destination = "${relativeDir}/${testPath}";
      in
      lib.optionalString (testPath != null) ''
        mkdir -p "$out"/${lib.escapeShellArg (builtins.dirOf destination)}
        if [ ! -f "$out"/${lib.escapeShellArg destination} ]; then
          printf 'fn main() {}\n' > "$out"/${lib.escapeShellArg destination}
        fi
      ''
    ) (builtins.attrNames entries)
  ) (craneLib.findCargoFiles self).cargoTomls;
  dummySource = craneLib.mkDummySrc {
    src = self;
    extraDummyScript = autoTests + extraDummyScript;
  };
  dependencies = package.overrideAttrs (old: {
    pname = "${old.pname}-dependencies";
    src = dummySource;
    outputs = [ "out" ];
    cargoArtifacts = null;
    # Revision identities belong to the real workspace compilation. Putting
    # them in this derivation would invalidate third-party artifacts every push.
    CLI_BUILD_STAMP = "";
    ST2_EXECUTOR_BUILD_IDENTITY = "";
    AGENT_SPEC_REVISION = "";
    ST3_MESSAGING_COMPAT_BIN = "";
    ST2_OTELITE_BIN = "";
    ST2_GITHUB_ISSUE_COMPONENT = "";
    ST2_GITHUB_PR_COMPONENT = "";
    ST2_PTY_STATS_COMPONENT = "";
    ST2_VISTA_COMPONENT = "";
    nativeCheckInputs = [ ];
    nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
      craneLib.installCargoArtifactsHook
      pkgs.zstd
    ];
    doCheck = false;
    checkPhase = "";
    # Use the same hooks, including their compiler/linker environment, to cache
    # dev dependencies without executing dummy tests.
    buildPhase =
      (old.buildPhase or "cargoBuildHook\n") + lib.optionalString old.doCheck "cargoCheckHook\n";
    cargoTestFlags = cargoFlags (old.cargoTestFlags or [ ]) ++ [ "--no-run" ];
    checkFlags = [ ];
    preCheck = "";
    postCheck = "";
    installPhase = ''
      # Cargo tests also build workspace binaries for CARGO_BIN_EXE. Never
      # inherit dummy executables or libraries into Nixpkgs' install staging
      # tree; dependency libraries and fingerprints live in the subdirectories.
      for profile in ${
        lib.escapeShellArgs (
          lib.unique [
            old.cargoBuildType
            old.cargoCheckType
          ]
        )
      }; do
        find target -mindepth 1 -maxdepth 2 -type d -name "$profile" \
          -exec find {} -maxdepth 1 -type f -delete \;
      done
      mkdir -p "$out"
      prepareAndInstallCargoArtifactsDir "$out"
    '';
    postInstall = "";
    postFixup = "";
  });
in
package.overrideAttrs (old: {
  cargoArtifacts = dependencies;
  # Nixpkgs copies vendored sources into a writable directory with fresh mtimes.
  # Build scripts track those files even for registry dependencies; newer
  # sources invalidate the restored artifacts (whose archive mtimes are 1).
  # Normalize only timestamps, preserving lock validation and all source bytes.
  postPatch = (old.postPatch or "") + ''
    find "$cargoDepsCopy" -type f -exec touch -h -d @1 {} +
  '';
  nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
    craneLib.inheritCargoArtifactsHook
    craneLib.installCargoArtifactsHook
    pkgs.zstd
  ];
  passthru = (old.passthru or { }) // {
    cargoDependencyArtifacts = dependencies;
  };
})
