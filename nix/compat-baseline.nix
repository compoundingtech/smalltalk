# Consume the published archive, not a separately rebuilt historical source.
{ pkgs, baseline }:
let
  target = pkgs.stdenv.hostPlatform.rust.rustcTarget;
  bundle = baseline.bundles.${target} or (throw "No ${target} bundle for compatibility baseline ${baseline.tag}");
in
pkgs.stdenv.mkDerivation {
  pname = "smalltalk-compat-baseline";
  version = pkgs.lib.removePrefix "v" baseline.tag;
  src = pkgs.fetchurl {
    inherit (bundle) url sha256;
  };
  nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.autoPatchelfHook ];
  buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.stdenv.cc.cc.lib ];
  dontBuild = true;
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/bin"
    cp bin/st3 "$out/bin/st3"
    cp BUILD.json "$out/BUILD.json"
    runHook postInstall
  '';
}
