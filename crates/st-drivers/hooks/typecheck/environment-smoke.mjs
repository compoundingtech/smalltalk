// Exercise rolling environment ownership without launching a provider or channel.
import assert from "node:assert/strict";
for (const [kind, file] of [["Pi", "pi"], ["Omp", "omp"]]) {
  const extension = await import(`./smoke-out/${file}-channel.mjs`);
  const prefix = `ST_${file.toUpperCase()}_CHANNEL_`;
  const fields = { BIN: "bin", CATALOG: "catalog", IDENTITY: "identity", RUNTIME_ID: "runtimeId", SESSION: "session", SEQ: "seq" };
  if (file === "omp") Object.assign(fields, { EXPECTED_NATIVE_SESSION: "expectedNativeSession", RESUME_GENERATION: "resumeGeneration" });
  const currentStash = `__st${kind}Channel`;
  const legacyStash = `__st2${kind}Channel`;
  const reset = () => {
    delete globalThis[currentStash];
    delete globalThis[legacyStash];
    for (const suffix of Object.keys(fields)) {
      delete process.env[prefix + suffix];
      delete process.env[`ST2_${prefix.slice(3)}${suffix}`];
    }
  };
  for (const mode of ["current", "legacy", "conflict", "empty", "reload"]) {
    reset();
    for (const suffix of Object.keys(fields)) {
      if (mode !== "legacy") process.env[prefix + suffix] = mode === "empty" ? "" : "current";
      if (mode !== "current") process.env[`ST2_${prefix.slice(3)}${suffix}`] = "legacy";
    }
    const held = Object.fromEntries(Object.values(fields).map(field => [field, "retained"]));
    if (mode === "reload") globalThis[legacyStash] = held;
    extension.default({ on() {} });
    const stash = globalThis[currentStash];
    for (const [suffix, field] of Object.entries(fields)) {
      assert.equal(stash[field], mode === "reload" ? "retained" : mode === "legacy" ? "legacy" : mode === "empty" ? "" : "current", `${file} ${mode} ${field}`);
      assert.equal(process.env[prefix + suffix], undefined, `${file} ${mode} removes current ${suffix}`);
      assert.equal(process.env[`ST2_${prefix.slice(3)}${suffix}`], undefined, `${file} ${mode} removes legacy ${suffix}`);
    }
    if (mode === "reload") assert.equal(stash, held, "reloading reuses channel ownership");
  }
  reset();
}
console.log("current, legacy, conflicting, empty, and reloaded environment ownership passed for pi and omp");
