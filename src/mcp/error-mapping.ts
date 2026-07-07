// mcp/error-mapping.ts — adapters between st's typed errors and the
// MCP CallToolResult shape.
//
// Per brief-009 implementation §"Error response shape" we settled on
// option C — both a human-readable prefixed text line AND a structured
// payload an embedder can pattern-match on. The SDK Client validates
// `structuredContent` against the tool's `outputSchema` whenever it's
// present (even on isError responses, despite the comment in the SDK
// suggesting otherwise), so we put the structured error payload under
// `_meta['st/error']` instead — `_meta` is reserved namespaced
// passthrough that does NOT get schema-validated.
//
// Concretely, every StError surfaces as:
//
//   {
//     isError: true,
//     content: [{ type: 'text', text: '<CODE>: <message>' }],
//     _meta: { 'st/error': { code, message, details? } }
//   }
//
// Embedders read `result._meta['st/error'].code`; older MCP clients
// still see a useful prefixed text line.

import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js';

import { StError } from '../errors.ts';

export const ST_ERROR_META_KEY = 'st/error' as const;

export interface StErrorPayload {
  code: string;
  message: string;
  details?: unknown;
}

/**
 * Map any thrown value to a CallToolResult. StError instances get
 * their stable code; anything else falls through to INTERNAL_ERROR.
 */
export function stErrorToToolResult(err: unknown): CallToolResult {
  const payload = errorPayload(err);
  return {
    isError: true,
    content: [
      { type: 'text', text: `${payload.code}: ${payload.message}` },
    ],
    _meta: {
      [ST_ERROR_META_KEY]: payload as unknown as Record<string, unknown>,
    },
  };
}

/**
 * Extract the structured error payload from a CallToolResult, if any.
 * Returns undefined for success results.
 */
export function readStErrorPayload(
  result: CallToolResult
): StErrorPayload | undefined {
  const meta = result._meta;
  if (!meta) return undefined;
  const payload = (meta as Record<string, unknown>)[ST_ERROR_META_KEY];
  if (typeof payload === 'object' && payload !== null) {
    return payload as unknown as StErrorPayload;
  }
  return undefined;
}

function errorPayload(err: unknown): StErrorPayload {
  if (err instanceof StError) {
    const payload: StErrorPayload = {
      code: err.code,
      message: err.message,
    };
    if (err.details !== undefined) payload.details = err.details;
    return payload;
  }
  if (err instanceof Error) {
    return { code: 'INTERNAL_ERROR', message: err.message };
  }
  return { code: 'INTERNAL_ERROR', message: String(err) };
}

/**
 * Happy-path constructor. Produces a CallToolResult with the human-
 * readable summary in `content[0].text` and the typed payload in
 * `structuredContent` (only when a value is supplied). Use when a tool
 * succeeds.
 */
export function buildToolResult<T extends Record<string, unknown>>(opts: {
  /** Human-readable one-line summary. */
  summary: string;
  /** Typed payload to expose as structuredContent. Omit for a
   *  no-result success (rare). */
  value?: T;
}): CallToolResult {
  const result: CallToolResult = {
    content: [{ type: 'text', text: opts.summary }],
  };
  if (opts.value !== undefined) {
    result.structuredContent = opts.value;
  }
  return result;
}

/**
 * Convenience: wrap an async tool body so any thrown StError or
 * Error is mapped to a structured error response. The body returns the
 * happy-path CallToolResult itself; rejections are caught here.
 */
export async function withErrorMapping(
  body: () => Promise<CallToolResult>
): Promise<CallToolResult> {
  try {
    return await body();
  } catch (err) {
    return stErrorToToolResult(err);
  }
}
