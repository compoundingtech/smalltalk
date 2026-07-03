// mcp/tools/context.ts — brief-024 context/ v1 MCP tools.
//
// Three dual-registered tools mirror the CLI verbs 1:1:
//   coord_context_read   / st_context_read
//   coord_context_write  / st_context_write
//   coord_context_append / st_context_append
//
// All three are absent-able: `read` on a missing folder returns
// `{ text: '', absent: true }`; `write` and `append` lazy-create the
// folder. This is what lets evals-claude's restart-continuity eval
// A/B a no-context control arm against a treatment arm without
// special-casing empty state.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import type { Coord } from '../../lib.ts';
import { asIdentity } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';
import { registerDualTool } from './dual-register.ts';

// ─── read ────────────────────────────────────────────────────────────────

const readInputShape = {
  identity: z
    .string()
    .optional()
    .describe(
      'Whose context/ folder to read from. Defaults to the server identity.'
    ),
  file: z
    .enum(['now', 'decisions', 'full'])
    .optional()
    .describe(
      "Which surface: 'now' (default) prints now.md; 'decisions' concatenates entries in context/decisions/ in filename-sort (= chronological) order; 'full' prints both under labeled headings."
    ),
};

const readOutputShape = {
  identity: z.string(),
  file: z.enum(['now', 'decisions', 'full']),
  text: z.string(),
  absent: z.boolean().describe(
    "True when the requested file(s) don't exist yet. Distinguishes 'no prior context' from 'empty file' — the eval's restart-continuity control arm uses this."
  ),
};

function registerContextReadTool(mcp: McpServer, coord: Coord): void {
  registerDualTool(
    mcp,
    'context_read',
    {
      title: 'Read agent context/ working-state',
      description:
        "Equivalent to `coord context read`. Return the per-agent durable working-state (brief-024, in-context-state leg of lossless-restart). Absent-able: missing files return { text: '', absent: true }.",
      inputSchema: readInputShape,
      outputSchema: readOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const input: Parameters<Coord['context']['read']>[0] = {};
        if (args.identity !== undefined) input.identity = asIdentity(args.identity);
        if (args.file !== undefined) input.file = args.file;
        const r = coord.context.read(input);
        return buildToolResult({
          summary: r.absent
            ? `context/${r.file} absent for ${r.identity}`
            : `context/${r.file} @ ${r.identity} (${r.text.length} bytes)`,
          value: {
            identity: r.identity,
            file: r.file,
            text: r.text,
            absent: r.absent,
          },
        });
      })
  );
}

// ─── write ───────────────────────────────────────────────────────────────

const writeInputShape = {
  body: z
    .string()
    .describe(
      "The whole-file content for now.md. Rewritten atomically (tmp + rename) so a concurrent SessionStart hook never sees a partial file. Trailing newline is added if absent."
    ),
  identity: z
    .string()
    .optional()
    .describe('Whose context/now.md to rewrite. Defaults to the server identity.'),
};

const writeOutputShape = {
  identity: z.string(),
  path: z.string(),
  bytes: z.number(),
};

function registerContextWriteTool(mcp: McpServer, coord: Coord): void {
  registerDualTool(
    mcp,
    'context_write',
    {
      title: 'Rewrite agent context/now.md',
      description:
        "Equivalent to `coord context write`. Replace now.md with `body`. Whole-file rewrite discipline — v1 rejects edit-in-place because it's how staleness creeps in. Lazy-creates the context/ folder.",
      inputSchema: writeInputShape,
      outputSchema: writeOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const input: Parameters<Coord['context']['write']>[0] = {
          body: args.body,
        };
        if (args.identity !== undefined) input.identity = asIdentity(args.identity);
        const r = coord.context.write(input);
        return buildToolResult({
          summary: `wrote ${r.bytes} bytes to ${r.path}`,
          value: { identity: r.identity, path: r.path, bytes: r.bytes },
        });
      })
  );
}

// ─── append ──────────────────────────────────────────────────────────────

const appendInputShape = {
  decision: z
    .string()
    .describe(
      "One-line decision. Multi-line reasoning belongs in a doc; this log stays scannable. Trailing period optional (we add one)."
    ),
  why: z
    .string()
    .describe(
      "One-line reason. The load-bearing part — this is what stops a restarted-you from re-litigating."
    ),
  timestamp: z
    .string()
    .optional()
    .describe(
      "Optional ISO timestamp override. Omit to stamp `new Date().toISOString()` at the moment of append."
    ),
  identity: z
    .string()
    .optional()
    .describe(
      "Whose context/decisions/ folder to append to. Defaults to the server identity."
    ),
};

const appendOutputShape = {
  identity: z.string(),
  path: z.string().describe(
    "Absolute path of the newly-written entry file: <coord-root>/<identity>/context/decisions/<unix-ms>-<rand6>.md."
  ),
  filename: z.string().describe(
    "Basename of the entry file (LAYOUT-004 grammar: <unix-ms>-<rand6>.md)."
  ),
  line: z.string().describe('The bulleted line stored in the file.'),
};

function registerContextAppendTool(mcp: McpServer, coord: Coord): void {
  registerDualTool(
    mcp,
    'context_append',
    {
      title: 'Append one decision to context/decisions/',
      description:
        "Equivalent to `coord context append --decision <text> --why <text>`. Creates a new file `<unix-ms>-<rand6>.md` under `<coord-root>/<identity>/context/decisions/` with one bulleted line. Append semantics = new file per entry — no rewriting an existing log, no clobbering on race. Lazy-creates the context/ + decisions/ folders.",
      inputSchema: appendInputShape,
      outputSchema: appendOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const input: Parameters<Coord['context']['append']>[0] = {
          decision: args.decision,
          why: args.why,
        };
        if (args.timestamp !== undefined) input.timestamp = args.timestamp;
        if (args.identity !== undefined) input.identity = asIdentity(args.identity);
        const r = coord.context.append(input);
        return buildToolResult({
          summary: `appended: ${r.filename}`,
          value: {
            identity: r.identity,
            path: r.path,
            filename: r.filename,
            line: r.line,
          },
        });
      })
  );
}

// ─── Public entry point ──────────────────────────────────────────────────

export function registerContextTools(mcp: McpServer, coord: Coord): void {
  registerContextReadTool(mcp, coord);
  registerContextWriteTool(mcp, coord);
  registerContextAppendTool(mcp, coord);
}
