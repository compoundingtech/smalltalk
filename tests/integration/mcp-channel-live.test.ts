// tests/integration/mcp-channel-live.test.ts — end-to-end channel demo.
//
// Spawns a real Claude Code session pointed at `st mcp --channel`,
// drops a coord message into the agent's inbox, and waits for the
// agent to call `st_msg_reply` so a reply file lands in the sender's
// inbox.
//
// Skip-gated by `COORD_RUN_LIVE_CLAUDE=1`. CI without Claude Code on
// `$PATH` (or without the `--dangerously-load-development-channels`
// flag) does not run this. The shape proof is what matters; we are
// not running this test on every commit.
//
// To run locally:
//   COORD_RUN_LIVE_CLAUDE=1 npx vitest run tests/integration/mcp-channel-live.test.ts
//
// Requires:
//   - `claude` on $PATH
//   - the build of this repo at bin/coord (the test points the channel
//     server at it)

import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { Session } from '@myobie/pty/testing';

import { COORD_BIN } from './helpers.ts';

const LIVE = process.env.COORD_RUN_LIVE_CLAUDE === '1';

let scratch: string;
let stRoot: string;
let homeDir: string;
let spawnCwd: string;
let mcpConfigJson: string;
let session: Session | undefined;
let ptySessionDir: string;
let claudeJsonBackup: string | null;
let claudeJsonPath: string;
let projectTranscriptDir: string;
let projectTranscriptDirPreExisted: boolean;

function cleanupSession(): void {
  if (session) {
    try {
      session.close();
    } catch {
      // best-effort
    }
    session = undefined;
  }
}

beforeEach(() => {
  if (!LIVE) return;
  if (!process.env.HOME) {
    throw new Error(
      'mcp-channel-live: $HOME must be set (keychain auth is HOME-gated)'
    );
  }

  // PTY_SESSION_DIR comes from the global setup at
  // tests/setup/pty-isolation.ts (brief-020). Refuse to run if it
  // didn't apply — the alternative is spawning Claude Code into the
  // user's real pty session dir.
  if (!process.env.PTY_SESSION_DIR) {
    throw new Error(
      'PTY_SESSION_DIR is not set — tests/setup/pty-isolation.ts ' +
        "did not run. Refusing to spawn Claude Code into the user's " +
        'real pty session dir.'
    );
  }
  ptySessionDir = process.env.PTY_SESSION_DIR;
  mkdirSync(ptySessionDir, { recursive: true });

  scratch = mkdtempSync(join(tmpdir(), 'coord-it-channel-'));
  stRoot = join(scratch, 'coord');
  for (const id of ['alice', 'bob']) {
    mkdirSync(join(stRoot, id, 'inbox'), { recursive: true });
    mkdirSync(join(stRoot, id, 'archive'), { recursive: true });
  }

  // We use the developer's real $HOME (not an isolated one) so:
  //   - keychain-based OAuth carries over (Claude's tokens live in
  //     macOS Keychain, not in ~/.claude.json — an isolated HOME
  //     means "Not logged in" and no LLM calls work);
  //   - whatever feature gate turns channels on (build channel, beta
  //     flags, etc.) is already set on the developer's account.
  // Per-test isolation comes from --mcp-config + --strict-mcp-config
  // (only our coord server is registered) plus a snapshot+restore of
  // ~/.claude.json around the run so the developer's normal Claude
  // state isn't mutated.
  homeDir = process.env.HOME;

  // Use the scratch dir itself as the spawn cwd. Trust is granted by
  // adding it to projects.<cwd>.hasTrustDialogAccepted in the
  // in-memory copy of ~/.claude.json we write below (the original is
  // restored on afterEach). Canonicalize via realpathSync because
  // Claude stores trust by the kernel-resolved path on macOS
  // (/var/folders → /private/var/...).
  const cwdRaw = join(scratch, 'spawn-cwd');
  mkdirSync(cwdRaw, { recursive: true });
  spawnCwd = realpathSync(cwdRaw);

  // Snapshot ~/.claude.json so the test can mutate it (adding the
  // scratch cwd to projects map) without leaking into the developer's
  // normal state. afterEach restores. If the file doesn't exist
  // (extremely unlikely on a dev machine that's run Claude Code),
  // we capture null and remove it on cleanup.
  claudeJsonPath = join(homeDir, '.claude.json');
  try {
    claudeJsonBackup = readFileSync(claudeJsonPath, 'utf8');
  } catch {
    claudeJsonBackup = null;
  }
  const claudeJson = JSON.parse(claudeJsonBackup ?? '{}') as Record<
    string,
    unknown
  >;
  const projects =
    (claudeJson.projects as Record<string, unknown> | undefined) ?? {};
  projects[spawnCwd] = {
    hasTrustDialogAccepted: true,
    hasCompletedProjectOnboarding: true,
  };
  claudeJson.projects = projects;
  writeFileSync(claudeJsonPath, JSON.stringify(claudeJson, null, 2));

  // The transcript dir Claude will create under
  // ~/.claude/projects/<encoded-cwd>/ — note whether it pre-existed
  // so we can clean it up unconditionally if not.
  const encoded = spawnCwd.replace(/\//g, '-');
  projectTranscriptDir = join(homeDir, '.claude', 'projects', encoded);
  projectTranscriptDirPreExisted = existsSync(projectTranscriptDir);

  // Build the inline MCP config string passed via --mcp-config. This
  // bypasses ~/.claude.json's mcpServers entirely (--strict-mcp-config
  // means "only these servers"). The ST_ROOT/ST_AGENT env
  // is passed to the coord subprocess via the env array.
  mcpConfigJson = JSON.stringify({
    mcpServers: {
      coord: {
        command: COORD_BIN,
        args: ['mcp', '--channel'],
        env: {
          ST_ROOT: stRoot,
          ST_AGENT: 'bob',
        },
      },
    },
  });
});

afterEach(() => {
  cleanupSession();
  if (!LIVE) return;
  // Restore ~/.claude.json from snapshot. Best-effort — if writing
  // fails we'd rather leak than crash the test runner.
  try {
    if (claudeJsonBackup !== null) {
      writeFileSync(claudeJsonPath, claudeJsonBackup);
    } else if (existsSync(claudeJsonPath)) {
      rmSync(claudeJsonPath);
    }
  } catch {
    // best-effort
  }
  // Remove the test's transcript dir (Claude created it during the
  // run). Skip if it pre-existed — won't happen with a scratch cwd
  // unless the path-hash collides with an existing project.
  try {
    if (!projectTranscriptDirPreExisted && existsSync(projectTranscriptDir)) {
      rmSync(projectTranscriptDir, { recursive: true, force: true });
    }
  } catch {
    // best-effort
  }
  rmSync(scratch, { recursive: true, force: true });
});

/**
 * Parse a coord-style markdown file's frontmatter + body. Returns the
 * raw frontmatter text (between the `---` fences) and the body after.
 * Returns null when the file isn't a valid frontmatter document.
 */
function parseFm(text: string): { fm: string; body: string } | null {
  const m =
    /^---\r?\n([\s\S]*?)\r?\n---\r?\n([\s\S]*)$/.exec(text);
  if (!m || m[1] === undefined || m[2] === undefined) return null;
  return { fm: m[1], body: m[2] };
}

async function pollForReply(
  recipient: string,
  predicate: (filename: string, parsed: { fm: string; body: string }) => boolean,
  timeoutMs: number
): Promise<{ filename: string; fm: string; body: string }> {
  const dir = join(stRoot, recipient, 'inbox');
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(dir)) {
      for (const name of readdirSync(dir)) {
        let text: string;
        try {
          text = readFileSync(join(dir, name), 'utf8');
        } catch {
          // Partial write or transient permissions error — skip and
          // retry on the next loop.
          continue;
        }
        const parsed = parseFm(text);
        if (parsed === null) continue;
        if (predicate(name, parsed)) {
          return { filename: name, fm: parsed.fm, body: parsed.body };
        }
      }
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(
    `pollForReply: no matching file in ${dir} within ${timeoutMs}ms`
  );
}

describe.skipIf(!LIVE)('mcp channel — live Claude Code agent', () => {
  it(
    'agent receives the channel notification and replies via st_msg_reply',
    async () => {
      // Spawn an interactive Claude Code session in a PTY. The
      // --dangerously-load-development-channels flag turns the coord
      // MCP server into a push channel for this session.
      session = Session.spawn(
        'claude',
        [
          '--strict-mcp-config',
          '--mcp-config',
          mcpConfigJson,
          '--dangerously-load-development-channels',
          'server:coord',
          '--dangerously-skip-permissions',
        ],
        {
          cwd: spawnCwd,
          env: {
            HOME: homeDir,
            PATH: process.env.PATH ?? '',
            PTY_SESSION_DIR: ptySessionDir,
            // Session.spawn merges opts.env on top of process.env, so
            // any COORD_/ST_ vars set on the runner leak into the
            // spawned agent's shell. If the agent then shells out to
            // `st message ls`, it reads the runner's real inbox
            // tree instead of the test scratch. Explicitly rebind to
            // the test tree so CLI paths match the MCP paths.
            ST_ROOT: stRoot,
            ST_ROOT: stRoot,
            ST_AGENT: 'bob',
            ST_IDENTITY: 'bob',
            ST_AGENT: 'bob',
          },
        }
      );
      // The first prompt Claude Code shows is the dev-channels warning:
      //   "I am using this for local development" / "Exit"
      // There's no flag to skip it — dismiss interactively. Then wait
      // for the actual session-boot completion marker.
      await session.waitForText(
        'I am using this for local development',
        20_000
      );
      session.press('return');
      // Post-boot settle. Older Claude Code builds surfaced a
      // "Listening for channel messages from: server:coord" banner we
      // could wait on; recent versions (v2.1.x observed 2026-07) don't
      // print it, so the boot marker is unreliable. We just sleep long
      // enough for the MCP handshake + channel-watcher subscribe to
      // complete, then let the message-drop step below prove liveness
      // end-to-end. If the channel is broken, the reply never lands
      // and the outer pollForReply times out.
      await new Promise((r) => setTimeout(r, 6000));

      // From alice (the test acting as alice), drop a question into
      // bob's inbox. The st_msg_reply tool's contract is "reply to the
      // <thread>'s sender" — that's alice.
      const original = '1714826789010-aaaaaa.md';
      writeFileSync(
        join(stRoot, 'bob', 'inbox', original),
        '---\nfrom: alice\nsubject: math\n---\n' +
          'what is 2+2? Please call the st_msg_reply tool with my message ' +
          'as the thread arg and your answer as the body.\n'
      );

      // Channels deliver into the agent's context, but a fresh idle
      // session may not auto-turn on the notification — it waits for
      // user input. Nudge the agent with a tiny user prompt so the
      // channel-delivered content participates in the next turn.
      await new Promise((r) => setTimeout(r, 1500));
      session.type('check your coord channel and respond');
      session.press('return');

      // Wait for a file to land in alice/inbox/ that is (a) different
      // from the original we wrote into bob/inbox, (b) `from: bob`
      // anchored to a frontmatter line start, (c) `in-reply-to:
      // <original>` anchored similarly, and (d) the body mentions "4"
      // as a standalone token (not "24 hours", etc.).
      const fromRe = /^from: bob$/m;
      const replyRe = new RegExp(`^in-reply-to: ${original}$`, 'm');
      const answerRe = /\b4\b/;
      let found: { filename: string; fm: string; body: string };
      try {
        found = await pollForReply(
          'alice',
          (filename, parsed) => {
            if (filename === original) return false;
            if (!fromRe.test(parsed.fm)) return false;
            if (!replyRe.test(parsed.fm)) return false;
            return answerRe.test(parsed.body);
          },
          120_000
        );
      } catch (err) {
        // Dump the session screen so we can see what Claude was doing.
        // eslint-disable-next-line no-console
        console.error(
          '[claude-live-test] poll failed; final session screen:\n' +
            session?.screenshot().text
        );
        throw err;
      }
      expect(found.fm).toMatch(fromRe);
      expect(found.fm).toMatch(replyRe);
      expect(found.body).toMatch(answerRe);
    },
    240_000
  );

  // brief-020 (HB-4) — the acceptance criterion. A coord message to
  // an IDLE agent (no user keystrokes) reliably surfaces and gets
  // processed. If this passes, the channel-watcher's polling backstop
  // + Claude Code's channel notification handling together deliver
  // "agents just get messages automatically". If this fails but the
  // poked-path test above passes, we've isolated the residual problem
  // to Claude Code's client-side idle-wake handling (hypothesis B/C
  // in the PR #27 write-up), and the asyncRewake hook in the
  // brief-020 follow-up becomes load-bearing.
  it(
    'brief-020: idle agent (no user input) auto-wakes on channel notification and replies',
    async () => {
      session = Session.spawn(
        'claude',
        [
          '--strict-mcp-config',
          '--mcp-config',
          mcpConfigJson,
          '--dangerously-load-development-channels',
          'server:coord',
          '--dangerously-skip-permissions',
        ],
        {
          cwd: spawnCwd,
          env: {
            HOME: homeDir,
            PATH: process.env.PATH ?? '',
            PTY_SESSION_DIR: ptySessionDir,
            ST_ROOT: stRoot,
            ST_ROOT: stRoot,
            ST_AGENT: 'bob',
            ST_IDENTITY: 'bob',
            ST_AGENT: 'bob',
          },
        }
      );
      await session.waitForText(
        'I am using this for local development',
        20_000
      );
      session.press('return');
      // Same 6s boot settle as the poked-path test.
      await new Promise((r) => setTimeout(r, 6000));

      // Drop the message. Same file/frontmatter as the poked test so
      // the reply predicate is identical.
      const original = '1714826789011-idlewk.md';
      writeFileSync(
        join(stRoot, 'bob', 'inbox', original),
        '---\nfrom: alice\nsubject: idle-wake test\n---\n' +
          'what is 2+2? Please call the st_msg_reply tool with my message ' +
          'as the thread arg and your answer as the body.\n'
      );

      // DO NOT type into the pty. The agent must wake purely from the
      // channel notification. 90s gives plenty of headroom for the
      // polling backstop (default 15s) + Claude's turn latency, even
      // if FSEvents drops the initial add event.
      const fromRe = /^from: bob$/m;
      const replyRe = new RegExp(`^in-reply-to: ${original}$`, 'm');
      const answerRe = /\b4\b/;
      let found: { filename: string; fm: string; body: string };
      try {
        found = await pollForReply(
          'alice',
          (filename, parsed) => {
            if (filename === original) return false;
            if (!fromRe.test(parsed.fm)) return false;
            if (!replyRe.test(parsed.fm)) return false;
            return answerRe.test(parsed.body);
          },
          90_000
        );
      } catch (err) {
        // eslint-disable-next-line no-console
        console.error(
          '[claude-live-test-idle] poll failed; final session screen:\n' +
            session?.screenshot().text
        );
        throw err;
      }
      expect(found.fm).toMatch(fromRe);
      expect(found.fm).toMatch(replyRe);
      expect(found.body).toMatch(answerRe);
    },
    180_000
  );
});
