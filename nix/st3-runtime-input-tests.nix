# Re-evaluate the actual flake outputs with one test dependency changed at a
# time, holding the source (and its embedded build stamp) fixed. The check must
# depend on both inputs, while the installed package must depend on neither.
{ self, system, inputs, baseline }:
let
  outputs = (import ../flake.nix).outputs;
  original = outputs (inputs // { inherit self; });
  vary = input: package:
    outputs (inputs // {
      inherit self;
      ${input} = inputs.${input} // {
        packages = inputs.${input}.packages // {
          ${system} = inputs.${input}.packages.${system} // {
            ${package} = inputs.${input}.packages.${system}.${package}.overrideAttrs (old: {
              name = "${old.name or "${old.pname}-${old.version}"}-check-input-successor";
            });
          };
        };
      };
    });
  collectorChanged = vary "effect-utils" "otelite";
  # The baseline is now a published archive, not a flake input. Vary the
  # actual archive wrapper at its test-only dependency boundary.
  baselineSuccessor = baseline.overrideAttrs (old: {
    name = "${old.name}-check-input-successor";
  });
  varyBaseline = derivation: derivation.overrideAttrs (old: {
    # overrideAttrs receives original inputs, before stdenv folds them.
    nativeCheckInputs = map
      (input: if input.outPath == baseline.outPath then baselineSuccessor else input)
      old.nativeCheckInputs;
  });
  runtime = result: result.packages.${system}.st3;
  checked = result: result.checks.${system}.st3;
in
assert (runtime original).outPath == (runtime collectorChanged).outPath;
assert (runtime original).outPath == (varyBaseline (runtime original)).outPath;
assert (checked original).outPath != (checked collectorChanged).outPath;
assert builtins.any (input: input.outPath == baseline.outPath) (checked original).nativeBuildInputs;
assert (checked original).outPath != (varyBaseline (checked original)).outPath;
assert !(runtime original).doCheck;
assert (checked original).doCheck;
true
