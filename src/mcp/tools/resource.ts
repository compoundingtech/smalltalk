// mcp/tools/resource.ts — registers the four `st_resource_*` MCP
// tools.
//
// add    — write a new resource under the agent's own identity
// ls     — list filenames in <identity>/resources/ (any identity)
// read   — return parsed Resource for one filename
// remove — delete a resource (own identity only)

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { z } from 'zod';

import type { Coord } from '../../lib.ts';
import { asFilename, asIdentity } from '../../types.ts';
import {
  buildToolResult,
  withErrorMapping,
} from '../error-mapping.ts';

// ─── add ───────────────────────────────────────────────────────────────

const addInputShape = {
  url: z
    .string()
    .describe(
      'URL the resource points at. Required. Must contain a scheme (e.g. `https://`, `pty://`).'
    ),
  title: z.string().optional().describe('Optional one-line title.'),
  tags: z
    .array(z.string())
    .optional()
    .describe('Optional list of tag strings.'),
  relation: z
    .string()
    .optional()
    .describe(
      'Optional free-form relation between the agent and the URL. Canonical (non-enforced) values: `owns` / `relates-to` / `depends-on`. Never inferred — absent by default. The bare URL stays first-class.'
    ),
  body: z
    .string()
    .optional()
    .describe('Optional markdown description body.'),
};

const addOutputShape = {
  filename: z.string(),
  identity: z.string(),
};

// ─── ls ────────────────────────────────────────────────────────────────

const lsInputShape = {
  identity: z
    .string()
    .optional()
    .describe('Whose resources to list. Defaults to $ST_AGENT.'),
};

const lsOutputShape = {
  identity: z.string(),
  resources: z.array(
    z.object({
      filename: z.string(),
      url: z.string(),
      title: z.string().nullable(),
      tags: z.array(z.string()),
      relation: z.string().nullable(),
    })
  ),
};

// ─── read ──────────────────────────────────────────────────────────────

const readInputShape = {
  filename: z.string().describe('Resource filename (LAYOUT-004 grammar).'),
  identity: z
    .string()
    .optional()
    .describe('Whose resource to read. Defaults to $ST_AGENT.'),
};

const readOutputShape = {
  identity: z.string(),
  filename: z.string(),
  url: z.string(),
  title: z.string().nullable(),
  tags: z.array(z.string()),
  relation: z.string().nullable(),
  body: z.string(),
};

// ─── remove ────────────────────────────────────────────────────────────

const removeInputShape = {
  filename: z
    .string()
    .describe('Resource filename to delete from your own resources/.'),
};

const removeOutputShape = {
  identity: z.string(),
  filename: z.string(),
  removed: z.boolean(),
};

// ─── Registrations ─────────────────────────────────────────────────────

export function registerResourceTools(mcp: McpServer, coord: Coord): void {
  mcp.registerTool(
    'st_resource_add',
    {
      title: 'Add a resource (annotated URL)',
      description:
        "Equivalent to `st resource add`. Write a new resource file under the agent's own resources/ folder. URL is required; title/tags/body are optional. The act of writing IS the publish; sync surfaces it to peers later.",
      inputSchema: addInputShape,
      outputSchema: addOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const filename = await coord.resources.add({
          url: args.url,
          ...(args.title !== undefined && { title: args.title }),
          ...(args.tags !== undefined && { tags: args.tags }),
          ...(args.relation !== undefined && { relation: args.relation }),
          ...(args.body !== undefined && { body: args.body }),
        });
        return buildToolResult({
          summary: `added: ${coord.identity}/${filename}`,
          value: { filename, identity: coord.identity },
        });
      })
  );

  mcp.registerTool(
    'st_resource_ls',
    {
      title: 'List an agent\'s resources',
      description:
        'Equivalent to `st resource ls`. Returns parsed resource records (filename + url + title + tags) for the given identity. Identity defaults to $ST_AGENT but any peer is readable.',
      inputSchema: lsInputShape,
      outputSchema: lsOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const target =
          args.identity !== undefined
            ? asIdentity(args.identity)
            : coord.identity;
        const items = await coord.resources.list(target);
        const resources = items.map((it) => ({
          filename: it.filename,
          url: it.resource.url,
          title: it.resource.title ?? null,
          tags: it.resource.tags ?? [],
          relation: it.resource.relation ?? null,
        }));
        const count = resources.length;
        const summary =
          count === 1 ? '1 resource' : `${count} resources`;
        return buildToolResult({
          summary,
          value: { identity: target, resources },
        });
      })
  );

  mcp.registerTool(
    'st_resource_read',
    {
      title: 'Read one of an agent\'s resources',
      description:
        'Equivalent to `st resource read`. Returns the parsed Resource (url + title + tags + body). Identity defaults to $ST_AGENT.',
      inputSchema: readInputShape,
      outputSchema: readOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const target =
          args.identity !== undefined
            ? asIdentity(args.identity)
            : coord.identity;
        const filename = asFilename(args.filename);
        const r = await coord.resources.read(target, filename);
        return buildToolResult({
          summary: `${target}/${filename}: ${r.url}`,
          value: {
            identity: target,
            filename,
            url: r.url,
            title: r.title ?? null,
            tags: r.tags ?? [],
            relation: r.relation ?? null,
            body: r.body,
          },
        });
      })
  );

  mcp.registerTool(
    'st_resource_remove',
    {
      title: 'Remove one of your own resources',
      description:
        "Equivalent to `st resource rm`. Deletes a resource from the agent's own resources/. Single-writer: only the resource owner can remove. Throws RESOURCE_NOT_FOUND if the filename doesn't exist.",
      inputSchema: removeInputShape,
      outputSchema: removeOutputShape,
    },
    async (args) =>
      withErrorMapping(async () => {
        const filename = asFilename(args.filename);
        await coord.resources.remove(filename);
        return buildToolResult({
          summary: `removed: ${coord.identity}/${filename}`,
          value: { identity: coord.identity, filename, removed: true },
        });
      })
  );
}
