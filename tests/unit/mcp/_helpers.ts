// tests/unit/mcp/_helpers.ts — shared test utilities for the MCP unit
// suite.

import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js';

export interface CallResult {
  isError?: boolean;
  content?: Array<{ type: string; text?: string }>;
  structuredContent?: Record<string, unknown>;
  _meta?: Record<string, unknown>;
}

export interface StErrorPayloadShape {
  code: string;
  message: string;
  details?: unknown;
}

/**
 * Read the structured StError payload from a tool result. The MCP
 * server places the error code/message/details under
 * `_meta['st/error']` (rather than `structuredContent`) so the
 * Client's outputSchema validator doesn't reject the response.
 */
export function errorPayload(
  r: CallResult | CallToolResult
): StErrorPayloadShape | undefined {
  const meta = (r as CallResult)._meta;
  if (!meta) return undefined;
  const payload = meta['st/error'];
  if (typeof payload === 'object' && payload !== null) {
    return payload as unknown as StErrorPayloadShape;
  }
  return undefined;
}

/** Convenience: the code from the error payload, or undefined. */
export function errorCode(
  r: CallResult | CallToolResult
): string | undefined {
  return errorPayload(r)?.code;
}
