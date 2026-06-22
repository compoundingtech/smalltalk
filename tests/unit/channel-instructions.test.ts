// tests/unit/channel-instructions.test.ts — pin every load-bearing
// substring in CHANNEL_INSTRUCTIONS so a careless refactor can't drop
// the boot ritual.
//
// brief-022 task 3: the channel-mode instructions string IS the
// contract an agent loads on every connection. The exact phrasing can
// move; the load-bearing verbs / tool names / commands cannot.

import { describe, expect, it } from 'vitest';

import { CHANNEL_INSTRUCTIONS } from '../../src/mcp/capabilities.ts';

describe('CHANNEL_INSTRUCTIONS — load-bearing substrings', () => {
  // Each entry is a substring the agent depends on being able to find.
  // Order doesn't matter; presence does.
  const REQUIRED_SUBSTRINGS = [
    // Status ritual
    'available',
    'coord status',
    // Inbox-drain tool surface
    'coord_msg_ls',
    'coord_msg_read',
    'coord_msg_archive',
    'coord_msg_reply',
    // Peer discovery
    'coord_members',
    // Task verbs (Phase 8 MCP tools stay parked; CLI shell-out is the contract)
    'coord task new',
    'coord task status',
    'coord task done',
    // Journal verb (brief-025 — narrative counterpart to tasks; CLI shell-out)
    'coord journal new',
    // Channel-arrival message format
    '<channel source="coord"',
    // Coord-threads-stay-on-coord rule: any thread originated via channel
    // / inbox is conversed via coord, not the REPL. Pins the load-bearing
    // phrases so the rule can't be silently weakened.
    'Coord threads stay on coord',
    'pty REPL is unattended',
  ] as const;

  for (const needle of REQUIRED_SUBSTRINGS) {
    it(`contains "${needle}"`, () => {
      expect(CHANNEL_INSTRUCTIONS).toContain(needle);
    });
  }

  it('is multi-paragraph (expanded beyond brief-010\'s one-sentence form)', () => {
    // Pre-022 the string was ~50 words on a single line. The expanded
    // boot-ritual form has multiple sections separated by blank lines.
    // Guard against a refactor that collapses it back.
    expect(CHANNEL_INSTRUCTIONS.split('\n').length).toBeGreaterThanOrEqual(10);
  });

  it('mentions tasks as the work-record concept', () => {
    // Soft-bound check: "task" is overloaded in software, so we look
    // for the verb-phrasing the ritual uses.
    expect(CHANNEL_INSTRUCTIONS).toMatch(/task file|task new|tasks\//i);
  });
});
