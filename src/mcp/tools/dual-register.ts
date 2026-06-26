// mcp/tools/dual-register.ts — register an MCP tool under both the
// legacy `coord_*` name and the new `st_*` name (brief-005-phase0 §3).
//
// Same schema, same handler, same description. Existing callers using
// `coord_msg_send` keep working; new callers can use `st_msg_send`.
// Phase 5 drops the `coord_*` registration once every config-at-rest
// has migrated.

import type { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';

type RegisterToolArgs = Parameters<McpServer['registerTool']>;

/**
 * Register an MCP tool under `coord_<baseName>` AND `st_<baseName>`.
 * Both registrations share the exact same `config` and `handler`
 * objects — the SDK handles each registration independently.
 */
export function registerDualTool(
  mcp: McpServer,
  baseName: string,
  config: RegisterToolArgs[1],
  handler: RegisterToolArgs[2]
): void {
  mcp.registerTool(`coord_${baseName}`, config, handler);
  mcp.registerTool(`st_${baseName}`, config, handler);
}
