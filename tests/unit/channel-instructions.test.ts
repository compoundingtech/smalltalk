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
    'st status',
    // Inbox-drain tool surface
    'st_msg_ls',
    'st_msg_read',
    'st_msg_archive',
    'st_msg_reply',
    // Peer discovery
    'st_agents',
    // Channel-arrival message format
    '<channel source="st"',
    // Smalltalk-threads-stay-on-smalltalk rule: any thread
    // originated via channel / inbox is conversed via the bus, not
    // the REPL. Pins the load-bearing phrases so the rule can't be
    // silently weakened.
    'Smalltalk threads stay on smalltalk',
    'pty REPL is unattended',
    // Coord is dead — the boot ritual shouldn't reference it
    // anywhere. Regression guard against the pre-cutover
    // instructions sneaking back in.
  ] as const;

  it('does not mention the retired `coord` name anywhere', () => {
    // Case-insensitive to catch "Coord" too.
    expect(CHANNEL_INSTRUCTIONS.toLowerCase()).not.toContain('coord');
  });

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
});
