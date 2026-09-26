// Local shim for the base PRODUCT package, @acme/signal.
//
// In the shipped package this file is just:
//     export * from "@acme/signal";
// i.e. a bare import of the peerDependency. In this hermetic seed tree there is
// no node_modules yet, so we re-export the base through a RELATIVE path instead.
//
// NOTE: this relative path — "../../signal/src/index.js" — is itself a PRODUCT
// rename site. The "signal" in it is the package DIRECTORY name (the product),
// so a correct product rename turns it into "../../beacon/src/index.js". It is
// NOT the runtime primitive. Product code in this package imports the base via
// "./_signal.js" so there is exactly one place to repoint.
export * from "../../signal/src/index.js";
