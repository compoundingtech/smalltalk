// Local shim for the base PRODUCT package, @acme/signal.
//
// In the shipped package this file is just:
//     export * from "@acme/signal";
// i.e. a bare import of the peerDependency. In this hermetic seed tree there is
// no node_modules yet, so we re-export the base through a RELATIVE path instead.
//
// NOTE: the relative path "../../signal/src/index.js" is a PRODUCT rename site —
// its "signal" is the base package DIRECTORY name (the product), which a correct
// rename turns into "../../beacon/src/index.js". It is NOT the runtime
// primitive. All product code here imports the base via "./_signal.js".
export * from "../../signal/src/index.js";
