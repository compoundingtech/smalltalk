#!/usr/bin/env node
// The `signal` product CLI. Thin wrapper — all logic lives in src/cli.js so it
// can be unit-tested. (PRODUCT binary: renamed to `beacon` along with the
// package. This file becomes bin/beacon.js and the package.json bin key flips.)

import { runCli } from "../src/cli.js";

const code = runCli(process.argv.slice(2));
process.exit(code);
