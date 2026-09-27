// CLI logic for the `signal` product binary, factored out so it is testable
// without spawning a process. bin/signal.js is a thin wrapper over runCli.

import { Bus, createSignal, PROTOCOL } from "./index.js";

const USAGE = `signal — the in-process signal bus (product)

usage:
  signal emit <name> [value]   emit a named product signal and print it
  signal serve [name ...]      start a tiny in-proc signal server, emit the
                               named signals once, then exit cleanly
  signal help                  show this help
`;

// runCli returns an exit code. `write` collects output so tests can assert it.
export function runCli(argv, { write = (s) => process.stdout.write(s) } = {}) {
  const [cmd, ...rest] = argv;

  switch (cmd) {
    case "emit": {
      const name = rest[0];
      if (!name) {
        write("error: emit needs a signal name\n");
        return 2;
      }
      const value = rest[1] ?? null;
      const sig = createSignal(name, value);
      write(`signal ${sig.name} [${sig.protocol}] = ${JSON.stringify(sig.value)}\n`);
      return 0;
    }

    case "serve": {
      const names = rest.length ? rest : ["ready"];
      const bus = new Bus();
      const seen = [];
      bus.on("*", () => {}); // no-op wildcard slot; kept for symmetry
      write(`signal server up (protocol ${PROTOCOL})\n`);
      for (const name of names) {
        bus.on(name, (s) => seen.push(s.name));
        bus.emitNamed(name, { at: seen.length });
        write(`  emitted signal: ${name}\n`);
      }
      write(`signal server done (${seen.length} signals)\n`);
      return 0;
    }

    case "help":
    case undefined:
      write(USAGE);
      return 0;

    default:
      write(`unknown command: ${cmd}\n\n${USAGE}`);
      return 2;
  }
}
