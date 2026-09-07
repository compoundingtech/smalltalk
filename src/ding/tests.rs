use super::*;
use crate::message::{archive_dir, archive_msg, inbox_dir, send_to_inbox};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

fn msg(filename: &str, from: &str, subject: Option<&str>) -> Message {
    Message {
        filename: filename.to_string(),
        ts_ms: filename
            .split_once('-')
            .and_then(|(timestamp, _)| timestamp.parse().ok())
            .unwrap_or_default(),
        from: Some(from.to_string()),
        subject: subject.map(str::to_string),
        in_reply_to: None,
        tags: vec![],
        priority: None,
        idempotency_key: None,
        stream: None,
        event_id: None,
        event_key: None,
        body: String::new(),
    }
}

fn declare_agent(root: &Path, host: &str, identity: &str, supervisor: Option<&str>) {
    let directory = root.join(host).join(identity);
    std::fs::create_dir_all(&directory).unwrap();
    let supervisor = supervisor
        .map(|value| format!("  supervisor {value:?}\n"))
        .unwrap_or_default();
    std::fs::write(
        directory.join("agent.kdl"),
        format!(
            "agent {identity:?} {{\n  identity {identity:?}\n  host {host:?}\n{supervisor}  type \"service\"\n  pty \"agent\" {{ command \"x\" }}\n}}\n"
        ),
    )
    .unwrap();
}

fn render_without_catalog(message: &Message) -> String {
    let catalog = tempfile::tempdir().unwrap();
    poke_text(catalog.path(), "h", "h.recipient", message)
}

fn notice_text_without_catalog(notice: &PendingNotice) -> String {
    let catalog = tempfile::tempdir().unwrap();
    notice.text(
        DingContext {
            catalog_root: catalog.path(),
            this_host: "h",
            recipient: "h.recipient",
        },
        &mut None,
    )
}

fn flush_without_catalog(
    status_path: Option<&Path>,
    pending: &mut VecDeque<PendingNotice>,
    poker: &dyn Poker,
) {
    let catalog = tempfile::tempdir().unwrap();
    flush_pending(
        DingContext {
            catalog_root: catalog.path(),
            this_host: "h",
            recipient: "h.recipient",
        },
        status_path,
        pending,
        poker,
    );
}

/// Block until `ready` holds, reporting whether it did. The ceiling bounds a DING loop that
/// made no progress at all; it is not the behaviour under test, so it is far larger than any
/// plausible scheduling delay and a loaded host cannot turn it into a failure. Callers report
/// the result after their scope ends rather than panicking inside it, because an unwind from a
/// scoped thread leaves `run_ding` without the `stop` flag that ends it.
fn await_ding_progress(ready: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    true
}

#[derive(Default)]
struct RecordingPoker {
    alive: AtomicBool,
    defer: AtomicBool,
    probes: AtomicUsize,
    failures: Mutex<usize>,
    calls: Mutex<Vec<String>>,
}

impl RecordingPoker {
    fn live() -> Self {
        Self {
            alive: AtomicBool::new(true),
            ..Default::default()
        }
    }
}

impl Poker for RecordingPoker {
    fn poke(&self, text: &str) -> anyhow::Result<PokeOutcome> {
        self.calls.lock().unwrap().push(text.to_string());
        if self.defer.load(Ordering::SeqCst) {
            return Ok(PokeOutcome::Deferred(DeferralReason::NoMaintainedComposer));
        }
        let mut failures = self.failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            anyhow::bail!("injected send failure");
        }
        Ok(PokeOutcome::Delivered)
    }

    fn session_alive(&self) -> bool {
        self.probes.fetch_add(1, Ordering::SeqCst);
        self.alive.load(Ordering::SeqCst)
    }
}

struct OwnershipPoker {
    pokes: Mutex<Vec<String>>,
    retries: Mutex<Vec<String>>,
    poke_outcomes: Mutex<VecDeque<PokeOutcome>>,
    retry_outcomes: Mutex<VecDeque<PokeOutcome>>,
}

impl Poker for OwnershipPoker {
    fn poke(&self, text: &str) -> anyhow::Result<PokeOutcome> {
        self.pokes.lock().unwrap().push(text.to_string());
        Ok(self
            .poke_outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected fresh poke"))
    }

    fn retry_staged(&self, text: &str) -> anyhow::Result<PokeOutcome> {
        self.retries.lock().unwrap().push(text.to_string());
        Ok(self
            .retry_outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected staged retry"))
    }

    fn session_alive(&self) -> bool {
        true
    }
}

#[test]
fn poke_id_extracts_rand6() {
    assert_eq!(poke_id("1785070000000-abc123.md"), "abc123");
    assert_eq!(poke_id("notes.md"), "notes");
}

#[test]
fn ancestor_depth_is_encoded_in_the_marker_run() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "h", "root", None);
    declare_agent(catalog.path(), "h", "middle", Some("h.root"));
    declare_agent(catalog.path(), "h", "recipient", Some("middle"));

    assert_eq!(
        poke_text(
            catalog.path(),
            "h",
            "h.recipient",
            &msg("1785070000000-abc123.md", "h.middle", Some("direct"))
        ),
        "[DING] ↓ h.middle: direct [id:abc123]"
    );
    assert_eq!(
        poke_text(
            catalog.path(),
            "h",
            "h.recipient",
            &msg("1785070000000-def456.md", "h.root", Some("skip-level"))
        ),
        "[DING] ↓↓ h.root: skip-level [id:def456]"
    );
}

#[test]
fn relationship_markers_cover_descendant_peer_and_missing_sender_spec() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "h", "root", None);
    declare_agent(catalog.path(), "h", "recipient", Some("root"));
    declare_agent(catalog.path(), "h", "child", Some("recipient"));
    declare_agent(catalog.path(), "h", "peer", Some("root"));

    assert_eq!(
        poke_text(
            catalog.path(),
            "h",
            "h.recipient",
            &msg("1785070000000-abc123.md", "h.child", Some("report"))
        ),
        "[DING] ↑ h.child: report [id:abc123]"
    );
    assert_eq!(
        poke_text(
            catalog.path(),
            "h",
            "h.recipient",
            &msg("1785070000000-def456.md", "h.peer", Some("note"))
        ),
        "[DING] ← h.peer: note [id:def456]"
    );
    assert_eq!(
        poke_text(
            catalog.path(),
            "h",
            "h.recipient",
            &msg("1785070000000-ghi789.md", "nightly-timer", Some("check"))
        ),
        "[DING] ? nightly-timer: check [id:ghi789]"
    );
}

#[test]
fn dangling_catalog_entry_makes_relationship_unknown() {
    use std::os::unix::fs::symlink;

    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "h", "root", None);
    declare_agent(catalog.path(), "h", "recipient", Some("root"));
    symlink(
        catalog.path().join("missing.kdl"),
        catalog.path().join("dangling.kdl"),
    )
    .unwrap();

    assert_eq!(
        poke_text(
            catalog.path(),
            "h",
            "h.recipient",
            &msg("1785070000000-abc123.md", "h.root", Some("dangling"))
        ),
        "[DING] ? h.root: dangling [id:abc123]"
    );
}

#[test]
fn self_addressed_message_uses_identity_marker() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "h", "recipient", None);

    assert_eq!(
        poke_text(
            catalog.path(),
            "h",
            "h.recipient",
            &msg("1785070000000-abc123.md", "h.recipient", Some("self"))
        ),
        "[DING] ↺ h.recipient: self [id:abc123]"
    );
}

#[test]
fn supervisor_cycle_renders_unknown_and_still_delivers() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "h", "recipient", Some("loop"));
    declare_agent(catalog.path(), "h", "loop", Some("recipient"));
    let message = msg("1785070000000-abc123.md", "h.loop", Some("cycle"));
    let expected = "[DING] ? h.loop: cycle [id:abc123]";
    let resolver = RelationshipResolver::read(catalog.path());
    let recipient = resolve_spec(&resolver.specs, "h.recipient", "h").unwrap();

    assert_eq!(
        chain_bus_ids(&resolver.specs, recipient, "h"),
        Err(crate::supervisor_chain::SupervisorChainError::Cycle),
        "cycle detection must be distinct from the independent depth limit"
    );

    assert_eq!(
        poke_text(catalog.path(), "h", "h.recipient", &message),
        expected
    );

    let mut pending = VecDeque::from([PendingNotice::message(message)]);
    let poker = RecordingPoker::live();
    flush_pending(
        DingContext {
            catalog_root: catalog.path(),
            this_host: "h",
            recipient: "h.recipient",
        },
        None,
        &mut pending,
        &poker,
    );

    assert!(pending.is_empty());
    assert_eq!(poker.calls.lock().unwrap().as_slice(), [expected]);
}

#[test]
fn malformed_catalog_with_resolvable_endpoints_renders_unknown_and_still_delivers() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "h", "root", None);
    declare_agent(catalog.path(), "h", "recipient", Some("root"));
    let malformed = catalog.path().join("h/broken/agent.kdl");
    std::fs::create_dir_all(malformed.parent().unwrap()).unwrap();
    std::fs::write(&malformed, "agent this is not valid KDL {").unwrap();
    let message = msg("1785070000000-abc123.md", "h.sender", Some("fallback"));
    let message = Message {
        from: Some("h.root".to_string()),
        ..message
    };
    let expected = "[DING] ? h.root: fallback [id:abc123]";
    let mut pending = VecDeque::from([PendingNotice::message(message)]);
    let poker = RecordingPoker::live();

    flush_pending(
        DingContext {
            catalog_root: catalog.path(),
            this_host: "h",
            recipient: "h.recipient",
        },
        None,
        &mut pending,
        &poker,
    );

    assert!(pending.is_empty());
    assert_eq!(poker.calls.lock().unwrap().as_slice(), [expected]);
}

#[test]
fn supervisor_depth_limit_fails_soft() {
    let catalog = tempfile::tempdir().unwrap();
    for depth in 0..=SUPERVISOR_CHAIN_LIMIT {
        let identity = format!("agent-{depth}");
        let supervisor =
            (depth < SUPERVISOR_CHAIN_LIMIT).then(|| format!("agent-{}", depth + 1));
        declare_agent(catalog.path(), "h", &identity, supervisor.as_deref());
    }

    assert_eq!(
        poke_text(
            catalog.path(),
            "h",
            "h.agent-0",
            &msg("1785070000000-abc123.md", "h.agent-64", Some("too deep"))
        ),
        "[DING] ? h.agent-64: too deep [id:abc123]"
    );
}

#[test]
fn poke_text_normalizes_and_bounds_untrusted_fields() {
    assert_eq!(
        render_without_catalog(&msg("1785070000000-abc123.md", "alice", Some("deploy?"))),
        "[DING] ? alice: deploy? [id:abc123]"
    );
    assert_eq!(
        render_without_catalog(&Message {
            from: None,
            subject: None,
            ..msg("1785070000000-def456.md", "", None)
        }),
        "[DING] ? unknown: (no subject) [id:def456]"
    );

    let subject = format!("{}\nignored", "s".repeat(SUBJECT_MAX_CHARS + 20));
    let sender = format!("{}\tignored", "f".repeat(SENDER_MAX_CHARS + 20));
    let text = render_without_catalog(&msg("1785070000000-ghi789.md", &sender, Some(&subject)));
    assert!(text.contains(&"s".repeat(SUBJECT_MAX_CHARS)));
    assert!(!text.contains(&"s".repeat(SUBJECT_MAX_CHARS + 1)));
    assert!(text.contains(&"f".repeat(SENDER_MAX_CHARS)));
    assert!(!text.contains(&"f".repeat(SENDER_MAX_CHARS + 1)));
    assert!(text.ends_with("[id:ghi789]"));
    assert!(text.contains("[id:ghi789]"));
}

#[test]
fn malicious_controls_cannot_escape_the_single_paste_frame() {
    let message = msg(
        "1785070000000-k0ygwh.md",
        "attacker\x1b[201~\r\u{009b}2J",
        Some("line one\n\tline two\x1b[201~key:return"),
    );
    let text = render_without_catalog(&message);
    assert!(!text.chars().any(char::is_control));
    assert!(!text.contains("  "));
    assert!(text.contains("[id:k0ygwh]"));
    assert!(text.ends_with("[id:k0ygwh]"));

    let direct = format!("{text}\x1b[201~\nsecond line");
    let args = pty_stage_args("seat", &direct);
    let framed = &args[3];
    assert_eq!(framed.matches(BRACKETED_PASTE_START).count(), 1);
    assert_eq!(framed.matches(BRACKETED_PASTE_END).count(), 1);
    let inner = framed
        .strip_prefix(BRACKETED_PASTE_START)
        .unwrap()
        .strip_suffix(BRACKETED_PASTE_END)
        .unwrap();
    assert!(!inner.chars().any(char::is_control));
    assert!(inner.ends_with("[201~ second line"));
}

#[test]
fn pty_stage_and_submit_are_separate_exact_sequences() {
    assert_eq!(
        pty_stage_args("my-session", "hello\nworld"),
        vec![
            "send",
            "my-session",
            "--seq",
            "\x1b[200~hello world\x1b[201~",
        ]
    );
    assert_eq!(
        pty_submit_args("my-session"),
        vec!["send", "my-session", "--seq", "key:return"]
    );
    assert!(!pty_stage_args("my-session", "hello").contains(&"key:return".to_string()));
}

#[test]
fn pty_delivery_uses_face607_delay_order_and_seconds() {
    assert_eq!(
        pty_delivery_args("s", "hello"),
        vec![
            "send",
            "s",
            "--with-delay",
            "0.5",
            "--seq",
            "\x1b[200~hello\x1b[201~",
            "--seq",
            "key:return"
        ]
    );
}

fn idle_codex_screen_with_footer(footer: &str) -> String {
    format!(
        "\x1b[1m›\x1b[1C\x1b[22;2mFind and fix a bug in @filename\r\n\r\n\
             \x1b[2C\x1b[0m{footer}"
    )
}

fn idle_codex_screen() -> String {
    idle_codex_screen_with_footer("gpt-5.6-sol xhigh · /workspace")
}

fn idle_codex_screen_with_home_relative_cwd() -> String {
    idle_codex_screen().replace(" · /workspace", " · ~/Code/st2")
}

fn staged_codex_screen_with_footer(text: &str, footer: &str) -> String {
    let rendered = text.replace(' ', "\x1b[1C");
    format!(
        "\x1b[1m›\x1b[1C\x1b[0m{rendered}\r\n\r\n\
             \x1b[2C\x1b[0m{footer}"
    )
}

fn staged_codex_screen(text: &str) -> String {
    staged_codex_screen_with_footer(text, "gpt-5.6-sol xhigh · /workspace")
}

fn staged_wrapped_codex_screen() -> (&'static str, String) {
    let text = "[DING] ↓ supervisor: a deliberately long synthetic notification with enough content to reach another renderer boundary before the final words [id:abc123]";
    let composer = concat!(
        "[DING] ↓ supervisor: a deliberately long synthetic notification with enough",
        "\x1b[3X\r\n",
        "  content to reach another renderer boundary before the final words [id:abc123]",
    );
    (
        text,
        format!(
            "\x1b[1m›\x1b[22m {composer}\r\n\r\n\x1b[2C\x1b[0mgpt-5.6-sol low · Context 14% used"
        ),
    )
}

fn human_codex_screen() -> String {
    staged_codex_screen("please keep my half-written draft")
}

fn accepted_codex_screen(text: &str) -> String {
    format!(
        "{}\r\n\r\n{}",
        staged_codex_screen(text),
        idle_codex_screen()
    )
}

fn queued_codex_screen(text: &str) -> String {
    format!(
        "Messages to be submitted after next tool call:\r\n{text}\r\n\r\n{}",
        idle_codex_screen()
    )
}

fn claude_rule() -> String {
    "─".repeat(80)
}

fn idle_claude_screen() -> String {
    let rule = claude_rule();
    format!(
        "Claude Code v2.1.220\r\n{rule}\r\n❯\u{00a0}Try \"write a test for validate.rs\"\r\n\
             {rule}\r\n  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents"
    )
}

fn idle_claude_screen_without_hint() -> String {
    idle_claude_screen().replace(" (shift+tab to cycle)", "")
}

fn staged_claude_screen(text: &str) -> String {
    assert!(text.is_ascii());
    let rule = claude_rule();
    let composer = if text.len() <= 77 {
        format!("❯\u{00a0}{text}")
    } else {
        let (first, continuation) = text.split_at(77);
        format!("❯\u{00a0}{first}\r\n  {continuation}")
    };
    format!(
        "Claude Code v2.1.220\r\n{rule}\r\n{composer}\r\n{rule}\r\n\
             ⏵⏵ bypass permissions on (shift+tab to cycle)"
    )
}

/// A pane that has been used: the startup banner has scrolled out of the peeked viewport, the
/// composer is empty rather than showing a rotating placeholder, and the footer omits the
/// conditional `(shift+tab to cycle)` hint while keeping the permission-mode indicator.
fn mature_claude_screen(composer: &str) -> String {
    let rule = claude_rule();
    let footer = "  ⏵⏵ bypass permissions on · PR #42 · 2 shells · ← 1 agent";
    format!("  an earlier turn\r\n{rule}\r\n{composer}\r\n{rule}\r\n{footer}")
}

fn mature_idle_claude_screen() -> String {
    mature_claude_screen("❯\u{00a0}")
}

fn mature_idle_claude_screen_with_hint() -> String {
    mature_idle_claude_screen().replace(
        "⏵⏵ bypass permissions on",
        "⏵⏵ bypass permissions on (shift+tab to cycle)",
    )
}

/// The same used pane in accept-edits mode. `permissions on` is specific to the bypass footer,
/// so no accept-edits or auto pane is positively idle to this classifier.
fn mature_idle_accept_edits_claude_screen() -> String {
    mature_idle_claude_screen().replace("⏵⏵ bypass permissions on", "⏵⏵ accept edits on")
}

fn mature_staged_claude_screen(text: &str) -> String {
    mature_claude_screen(&format!("❯\u{00a0}{text}"))
}

/// Selection region captured with `pty peek --plain st2-codex-selection-fixture` from a
/// dedicated 80x24 Codex CLI 0.145.0 pane on 2026-08-04. Codex rendered this built-in trust
/// selection after `codex --no-alt-screen -a untrusted -s read-only` launched in a fresh
/// untrusted directory. The command used no bypass flag. The leading working-directory row is
/// intentionally omitted; every retained row is verbatim from the captured selection.
const CAPTURED_CODEX_TRUST_SELECTION: &str = "
  Do you trust the contents of this directory? Working with untrusted contents
  comes with higher risk of prompt injection. Trusting the directory allows
  project-local config, hooks, and exec policies to load.

› 1. Yes, continue
  2. No, quit

  Press enter to continue";

/// Exact public-safe suffix from default raw `pty peek` while Codex CLI 0.145.0's `/model`
/// picker was open. It begins at the picker heading and preserves every emitted byte through
/// the end of stdout.
const CAPTURED_CODEX_MODEL_PICKER: &str =
    include_str!("fixtures/codex-model-picker-default-to-end.bin");

const CODEX_LATENCY_RETRY_NOTICE: &str =
    "Our systems are thinking a bit more\r\n\r\nRetry with a faster model";

/// Form region captured with `pty peek --plain st2-claude-form-fixture-clean` from a dedicated
/// 80x24 Claude Code 2.1.220 pane in safe/manual mode on 2026-08-03. Claude rendered this after
/// an `AskUserQuestion` call. The host-local banner and working directory are intentionally
/// omitted; every retained row is verbatim from the captured form.
const CAPTURED_CLAUDE_QUESTION_FORM: &str = "
────────────────────────────────────────────────────────────────────────────────
☐ Fixture

Which fixture should this test use?

❯ 1. Captured form
     Use a captured form fixture
  2. Plain composer
     Use a plain composer fixture\u{20}
  3. Type something.
────────────────────────────────────────────────────────────────────────────────
  4. Chat about this

Enter to select · ↑/↓ to navigate · Esc to cancel";

fn captured_claude_question_form_with_selection(option: usize) -> String {
    assert!((1..=4).contains(&option));
    if option == 1 {
        return CAPTURED_CLAUDE_QUESTION_FORM.to_string();
    }
    CAPTURED_CLAUDE_QUESTION_FORM
        .replacen("❯ 1.", "  1.", 1)
        .replacen(&format!("  {option}."), &format!("❯ {option}."), 1)
}

/// The captured form reduced only by deleting its third and fourth option rows. This pins the
/// real two-option shape without adding any screen layout that was not present in the capture.
fn captured_two_option_claude_question_form() -> String {
    CAPTURED_CLAUDE_QUESTION_FORM.replace(
        &format!(
            "  3. Type something.\n{}\n  4. Chat about this\n\n",
            claude_rule()
        ),
        "",
    )
}

/// The same used pane with an in-flight turn: a spinner status line above the composer. Every
/// frame below was observed on a real 2.1.220 pane; the glyph animates and the elapsed timer is
/// not always rendered, so both variations appear here.
fn mid_turn_claude_screen(status: &str, composer: &str) -> String {
    let rule = claude_rule();
    let footer = "  ⏵⏵ bypass permissions on · PR #42 · 2 shells · ← 1 agent";
    format!("  an earlier turn\r\n{status}\r\n{rule}\r\n{composer}\r\n{rule}\r\n{footer}")
}

/// Status lines for an ACTIVE turn — Return must never be sent.
const ACTIVE_TURN_STATUS: [&str; 4] = [
    "✻ Frolicking… (3m 35s · ↓ 6.9k tokens)",
    "✽ Schlepping…",
    "· Metamorphosing…",
    "✶ Schlepping… (9s · ↓ 296 tokens · thinking with high effort)",
];

/// Status lines for a FINISHED turn — these sit above every genuinely idle composer, so
/// treating them as blocked would stop delivery entirely.
const FINISHED_TURN_STATUS: [&str; 4] = [
    "✻ Brewed for 5s",
    "✻ Crunched for 7s",
    "✻ Cogitated for 11s · 1 shell still running",
    "✻ Baked for 3s · 1 shell still running",
];

/// A live Claude composer with a stale Codex composer above it in scrollback, preceded by
/// escape-heavy output. The escapes inflate the Codex byte offset far past the Claude
/// composer's row, which is what makes the two locators' units observably disagree.
fn live_claude_below_escape_heavy_codex_transcript() -> String {
    let padding =
        "\x1b[1;32m\x1b[38;5;204mpadding with lots of escapes\x1b[0m\x1b[0m\r\n".repeat(10);
    let codex = staged_codex_screen("a stale pasted codex draft");
    let filler = "\x1b[1;32mmore padding\x1b[0m\r\n".repeat(6);
    format!(
        "{padding}{codex}\r\n{filler}{}",
        mature_idle_claude_screen()
    )
}

/// A Codex pane whose scrollback holds a captured Claude screen — two ruled lines around a `❯`
/// row plus a Claude idle footer — above the live, ANSI-detected Codex composer. Capturing and
/// pasting pane text is routine, so this shape is not exotic.
fn codex_screen_below_claude_transcript(transcript_row: &str, codex: &str) -> String {
    let rule = claude_rule();
    format!(
        "  scrollback: a pasted Claude pane\r\n{rule}\r\n❯\u{00a0}{transcript_row}\r\n{rule}\r\n\
             \u{0020} ⏵⏵ bypass permissions on (shift+tab to cycle)\r\n\r\n{codex}"
    )
}

/// An in-flight Claude turn ships no interrupt hint, so the composer being empty is not proof
/// that Return is safe — the pane is working. The finished-turn line looks almost identical and
/// sits above every genuinely idle composer, so both directions are pinned here.
#[test]
fn an_in_flight_turn_blocks_return_but_a_finished_one_does_not() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";

    for status in ACTIVE_TURN_STATUS {
        // Empty composer mid-turn: positively empty, but not safe.
        assert_ne!(
            classify_composer(&mid_turn_claude_screen(status, "❯\u{00a0}"), expected),
            ComposerState::EmptySafe,
            "active turn must not be EmptySafe: {status}"
        );
        // The notice already staged mid-turn: exact, but Return is still withheld.
        assert_eq!(
            classify_composer(
                &mid_turn_claude_screen(status, &format!("❯\u{00a0}{expected}")),
                expected
            ),
            ComposerState::ExactBlocked,
            "active turn must be ExactBlocked: {status}"
        );
    }

    // A finished turn is the normal idle screen. Blocking on it would stop delivery forever.
    for status in FINISHED_TURN_STATUS {
        assert_eq!(
            classify_composer(&mid_turn_claude_screen(status, "❯\u{00a0}"), expected),
            ComposerState::EmptySafe,
            "finished turn must stay deliverable: {status}"
        );
        assert_eq!(
            classify_composer(
                &mid_turn_claude_screen(status, &format!("❯\u{00a0}{expected}")),
                expected
            ),
            ComposerState::ExactSafe,
            "finished turn must stay submittable: {status}"
        );
    }
}

#[test]
fn claude_question_form_blocks_while_an_ordinary_idle_composer_stays_deliverable() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";

    assert!(composer::looks_like_choice_menu(
        CAPTURED_CLAUDE_QUESTION_FORM
    ));
    assert_eq!(
        classify_composer(
            &format!(
                "{CAPTURED_CLAUDE_QUESTION_FORM}\r\n{}",
                mature_staged_claude_screen(expected)
            ),
            expected
        ),
        ComposerState::ExactBlocked
    );

    assert_eq!(
        classify_composer(&mature_idle_claude_screen(), expected),
        ComposerState::EmptySafe
    );
    assert_eq!(
        classify_composer(&mature_staged_claude_screen(expected), expected),
        ComposerState::ExactSafe
    );
}

#[test]
fn codex_trust_selection_blocks_while_an_ordinary_idle_composer_stays_deliverable() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";

    assert!(composer::looks_like_choice_menu(
        CAPTURED_CODEX_TRUST_SELECTION
    ));
    assert_eq!(
        classify_composer(CAPTURED_CODEX_TRUST_SELECTION, expected),
        ComposerState::Ambiguous
    );
    assert_eq!(
        classify_composer(
            &format!(
                "{CAPTURED_CODEX_TRUST_SELECTION}\r\n{}",
                staged_codex_screen(expected)
            ),
            expected
        ),
        ComposerState::ExactBlocked
    );

    assert_eq!(
        classify_composer(&idle_codex_screen(), expected),
        ComposerState::EmptySafe
    );
    assert_eq!(
        classify_composer(&staged_codex_screen(expected), expected),
        ComposerState::ExactSafe
    );
}

#[test]
fn captured_codex_model_picker_is_recognized_as_a_choice_menu() {
    let plain = composer::strip_ansi(CAPTURED_CODEX_MODEL_PICKER);

    assert!(composer::looks_like_choice_menu(&plain));
    assert_eq!(
        classify_composer(CAPTURED_CODEX_MODEL_PICKER, "unused notice"),
        ComposerState::Ambiguous
    );

    let without_selected_option = plain.replacen("› 1.", "  1.", 1);
    assert!(!composer::looks_like_choice_menu(&without_selected_option));
}

#[test]
fn fresh_delivery_does_not_return_after_idle_changes_to_model_picker() {
    use std::os::unix::fs::PermissionsExt as _;

    let text = "[DING] unread st2 messages remain; check your inbox";
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("pty");
    let idle = temp.path().join("idle.bin");
    let picker = temp.path().join("picker.bin");
    let changed = temp.path().join("changed");
    let audit = temp.path().join("audit.log");
    std::fs::write(&idle, idle_codex_screen()).unwrap();
    std::fs::write(&picker, CAPTURED_CODEX_MODEL_PICKER).unwrap();
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n\
                 if [ \"$1\" = peek ]; then\n\
                   if [ -e '{}' ]; then cat '{}'; else cat '{}'; fi\n\
                   exit 0\n\
                 fi\n\
                 if [ \"$1\" = send ]; then : > '{}'; exit 0; fi\n\
                 exit 1\n",
            audit.display(),
            changed.display(),
            picker.display(),
            idle.display(),
            changed.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    let poker = PtyPoker {
        bin: bin.to_string_lossy().into_owned(),
        session: "fixture".to_string(),
    };
    assert_eq!(
        poker.poke_with(text, &mut || Ok(())).unwrap(),
        PokeOutcome::Staged
    );

    let audit = std::fs::read_to_string(audit).unwrap();
    assert_eq!(audit.lines().next(), Some("peek fixture"));
    assert!(
        !audit.contains("key:return"),
        "Return must be withheld after the staged paste exposes a picker: {audit:?}"
    );
}

#[test]
fn codex_latency_retry_notice_blocks_without_choice_menu_structure() {
    let expected = "[DING] ? cos: latency control [id:abc123]";

    assert!(!composer::looks_like_choice_menu(
        CODEX_LATENCY_RETRY_NOTICE
    ));
    assert_eq!(
        classify_composer(
            &format!(
                "{CODEX_LATENCY_RETRY_NOTICE}\r\n{}",
                staged_codex_screen(expected)
            ),
            expected,
        ),
        ComposerState::ExactBlocked
    );
}

#[test]
fn claude_question_form_blocks_after_selection_moves_to_second_or_last_option() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";

    for option in [2, 4] {
        let form = captured_claude_question_form_with_selection(option);
        assert!(
            composer::looks_like_choice_menu(&form),
            "selection on option {option} must remain modal"
        );
        assert_eq!(
            classify_composer(
                &format!("{form}\r\n{}", mature_staged_claude_screen(expected)),
                expected
            ),
            ComposerState::ExactBlocked,
            "selection on option {option} must block Return"
        );
    }
}

#[test]
fn two_option_claude_question_form_blocks_return() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";
    let form = captured_two_option_claude_question_form();

    assert!(!form.contains("  3."));
    assert!(!form.contains("  4."));
    assert!(composer::looks_like_choice_menu(&form));
    assert_eq!(
        classify_composer(
            &format!("{form}\r\n{}", mature_staged_claude_screen(expected)),
            expected
        ),
        ComposerState::ExactBlocked
    );
}

#[test]
fn legacy_numbered_choice_menu_still_blocks_return() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";
    let legacy_menu = "› 1. Continue\r\n  2. Cancel";

    assert!(composer::looks_like_choice_menu(legacy_menu));
    assert_eq!(
        classify_composer(
            &format!("{legacy_menu}\r\n{}", mature_staged_claude_screen(expected)),
            expected
        ),
        ComposerState::ExactBlocked
    );
}

/// The two locators do not natively work in the same units. Codex is matched with `rfind` over
/// the raw screen, so it reports a **byte offset** inflated by every escape sequence above it;
/// Claude is matched over stripped lines, so it reports a **row**. Comparing those directly
/// picks Codex almost always, since an offset dwarfs a row — including when the live composer
/// is Claude's and the Codex match is stale scrollback. Both must be normalized to a row.
///
/// On the screen below, measured: the Codex composer sits at byte offset 560 but row 10, while
/// the live Claude composer is row 20. Comparing row against offset picks Codex, so this would
/// classify from a pasted draft instead of the real composer.
#[test]
fn composer_positions_are_compared_as_rows_not_raw_byte_offsets() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";
    assert_eq!(
        classify_composer(&live_claude_below_escape_heavy_codex_transcript(), expected),
        ComposerState::EmptySafe
    );
}

/// Normalizing the Codex offset means counting newlines in the *stripped* prefix, which is only
/// faithful if stripping preserves them. It does for well-formed input. It does not for an
/// unterminated sequence: the CSI scanner runs until a byte in `0x40..=0x7e` and `\n` is `0x0a`,
/// so it eats newlines, and an unterminated OSC consumes to the end of input. Both are recorded
/// here so a future change to `strip_ansi` cannot silently shift every row.
#[test]
fn stripping_preserves_newlines_for_well_formed_sequences_only() {
    let nl = |text: &str| composer::strip_ansi(text).matches('\n').count();

    assert_eq!(
        nl("\x1b[1;32mone\x1b[0m\r\n\x1b[2Ctwo\x1b[0m\r\n\x1b[1mthree\x1b[0m\r\n"),
        3
    );
    // Unterminated CSI: the newline is consumed while hunting for a final byte.
    assert_eq!(nl("before\r\n\x1b[999999\r\nafter\r\n"), 2);
    // Unterminated OSC: everything to the end of input is consumed.
    assert_eq!(nl("before\r\n\x1b]0;no terminator\r\nafter\r\n"), 1);
}

/// Scrollback that merely looks like a composer must never outrank the live one. The paste and
/// the Return always go to the pane's real bottom composer, so misreading transcript text as
/// "idle" or "already staged" is a wrong positive: it can type into, or submit, a human draft.
#[test]
fn transcript_composers_never_outrank_the_live_bottom_composer() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";

    // The live Codex composer holds a human draft in both cases, so both must stay `Changed`.
    // An empty transcript row would otherwise read as positively-empty and allow the paste.
    assert_eq!(
        classify_composer(
            &codex_screen_below_claude_transcript("", &human_codex_screen()),
            expected
        ),
        ComposerState::Changed
    );
    // A transcript row holding the exact notice is the worse case: it would otherwise satisfy
    // the two adjacent exact observations and send a bare Return to the draft.
    assert_eq!(
        classify_composer(
            &codex_screen_below_claude_transcript(expected, &human_codex_screen()),
            expected
        ),
        ComposerState::Changed
    );

    // The rule is positional, not a Codex preference: a genuine Claude pane whose scrollback
    // shows a captured Codex composer still classifies from its own live Claude composer.
    assert_eq!(
        classify_composer(
            &format!(
                "{}\r\n{}",
                staged_codex_screen("a stale pasted codex draft"),
                mature_idle_claude_screen()
            ),
            expected
        ),
        ComposerState::EmptySafe
    );
}

#[test]
fn maintained_composer_classifiers_require_exact_idle_state() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";
    assert_eq!(
        classify_composer(&idle_codex_screen(), expected),
        ComposerState::EmptySafe
    );
    assert_eq!(
        classify_composer(&idle_codex_screen_with_home_relative_cwd(), expected),
        ComposerState::EmptySafe
    );
    assert_eq!(
        classify_composer(&staged_codex_screen(expected), expected),
        ComposerState::ExactSafe
    );
    assert_eq!(
        classify_composer(&human_codex_screen(), expected),
        ComposerState::Changed
    );
    assert_eq!(
        classify_composer(
            &format!("Create a plan?\r\n{}", staged_codex_screen(expected)),
            expected
        ),
        ComposerState::ExactBlocked
    );

    assert_eq!(
        classify_composer(&idle_claude_screen(), expected),
        ComposerState::EmptySafe
    );
    assert_eq!(
        classify_composer(&idle_claude_screen_without_hint(), expected),
        ComposerState::EmptySafe
    );
    assert_eq!(
        classify_composer(&staged_claude_screen(expected), expected),
        ComposerState::ExactSafe
    );
    assert_eq!(
        classify_composer(&staged_claude_screen("a changed human composer"), expected),
        ComposerState::Changed
    );
    assert_eq!(
        classify_composer(
            &format!("Esc to interrupt\r\n{}", staged_claude_screen(expected)),
            expected
        ),
        ComposerState::ExactBlocked
    );
    // A used pane must classify exactly like a fresh one: neither the scrolled-away banner, the
    // empty composer, nor the missing cycle hint is evidence that Return is unsafe.
    assert_eq!(
        classify_composer(&mature_idle_claude_screen(), expected),
        ComposerState::EmptySafe
    );
    assert_eq!(
        classify_composer(&mature_idle_claude_screen_with_hint(), expected),
        ComposerState::EmptySafe
    );
    // Only the bypass footer carries `permissions on`, so an otherwise identical accept-edits
    // pane is never positively idle. It stays unsubmitted rather than being proven safe.
    assert_eq!(
        classify_composer(&mature_idle_accept_edits_claude_screen(), expected),
        ComposerState::Changed
    );
    assert_eq!(
        classify_composer(&mature_staged_claude_screen(expected), expected),
        ComposerState::ExactSafe
    );
    // The same pane shape must still fail closed on a human draft and on an active turn.
    assert_eq!(
        classify_composer(
            &mature_staged_claude_screen("a changed human composer"),
            expected
        ),
        ComposerState::Changed
    );
    assert_eq!(
        classify_composer(
            &format!(
                "Esc to interrupt\r\n{}",
                mature_staged_claude_screen(expected)
            ),
            expected
        ),
        ComposerState::ExactBlocked
    );

    assert_eq!(
        classify_composer("unknown terminal pixels", expected),
        ComposerState::Ambiguous
    );
}

#[test]
fn maintained_codex_context_footers_are_narrow_and_position_bound() {
    let expected = "[DING] ? cos: exact observation [id:abc123]";

    for footer in [
        "gpt-5.6-sol xhigh · ding-fix · Context 73% left",
        "gpt-5.6-sol xhigh · Context 0% used",
        "gpt-5.6-sol xhigh · Context 100% left",
    ] {
        assert_eq!(
            classify_composer(&idle_codex_screen_with_footer(footer), expected),
            ComposerState::EmptySafe,
            "maintained empty composer footer: {footer}"
        );
        assert_eq!(
            classify_composer(&staged_codex_screen_with_footer(expected, footer), expected),
            ComposerState::ExactSafe,
            "maintained staged composer footer: {footer}"
        );
    }

    for footer in [
        "gpt-5.6-sol xhigh · ding-fix",
        "gpt-5.6-sol xhigh · Contextual 73% left",
        "gpt-5.6-sol xhigh · Context 73 left",
        "gpt-5.6-sol xhigh · Context 101% left",
        "gpt-5.6-sol xhigh · Context left 73%",
        "gpt-5.6-sol xhigh · Context 73% remaining",
    ] {
        assert_eq!(
            classify_composer(&idle_codex_screen_with_footer(footer), expected),
            ComposerState::Ambiguous,
            "unsupported empty composer footer: {footer}"
        );
        assert_eq!(
            classify_composer(&staged_codex_screen_with_footer(expected, footer), expected),
            ComposerState::ExactBlocked,
            "unsupported staged composer footer: {footer}"
        );
    }

    let maintained_footer = "gpt-5.6-sol xhigh · ding-fix · Context 73% left";
    let branch_only_footer = "gpt-5.6-sol xhigh · ding-fix";
    assert_eq!(
        classify_composer(
            &format!(
                "{maintained_footer}\r\n{}",
                staged_codex_screen_with_footer(expected, branch_only_footer)
            ),
            expected
        ),
        ComposerState::ExactBlocked,
        "a valid-looking transcript row above the live composer is not its footer"
    );
    assert_eq!(
        classify_composer(
            &staged_codex_screen_with_footer(
                "please keep my half-written draft",
                maintained_footer
            ),
            expected
        ),
        ComposerState::Changed
    );
    for footer in [
        format!("{maintained_footer}\r\n{maintained_footer}"),
        format!("{maintained_footer}\r\n{branch_only_footer}"),
        format!("{maintained_footer}\r\nunknown trailing chrome"),
    ] {
        assert_eq!(
            classify_composer(&idle_codex_screen_with_footer(&footer), expected),
            ComposerState::Ambiguous,
            "duplicated or trailing footer chrome must not prove an empty composer idle: {footer}"
        );
        assert_eq!(
            classify_composer(
                &staged_codex_screen_with_footer(expected, &footer),
                expected
            ),
            ComposerState::ExactBlocked,
            "duplicated or trailing footer chrome must not prove Return safe: {footer}"
        );
    }
    for blocking_chrome in [
        "Esc to interrupt",
        "Create a plan?",
        "› 1. Allow\r\n  2. Deny",
    ] {
        assert_eq!(
            classify_composer(
                &format!(
                    "{blocking_chrome}\r\n{}",
                    staged_codex_screen_with_footer(expected, maintained_footer)
                ),
                expected
            ),
            ComposerState::ExactBlocked,
            "blocking chrome: {blocking_chrome}"
        );
    }
}

#[test]
fn codex_renderer_wraps_preserve_possible_inter_word_spaces() {
    let (expected, screen) = staged_wrapped_codex_screen();
    let token_split = expected.replace("enough content", "enoughcontent");
    let changed = expected.replace("synthetic", "different");

    assert_eq!(
        classify_composer(&screen, expected),
        ComposerState::ExactSafe
    );
    assert_eq!(
        classify_receipt(&screen, expected),
        ReceiptState::RetainedSafe
    );
    assert_eq!(
        classify_composer(&screen, &token_split),
        ComposerState::ExactSafe
    );
    assert_eq!(classify_composer(&screen, &changed), ComposerState::Changed);
    assert_eq!(
        classify_receipt(&screen, &changed),
        ReceiptState::NotRetained
    );
}

#[test]
fn codex_word_wraps_preserve_short_rows_before_continuations() {
    let expected = "[DING] new st2 message: [id:7j8b6n] re: Q97 light-work receipt: VRS/source consistency complete (from dev3.compoundingtech.st2.message-sent.orchestration); check your inbox";
    let screen = "\x1b[1m›\x1b[1C\x1b[0m[DING] new st2 message: [id:7j8b6n] re: Q97 light-work receipt: VRS/source\r\n  consistency complete (from dev3.compoundingtech.st2.message-\r\n  sent.orchestration); check your inbox\r\n\r\n\x1b[2C\x1b[0mgpt-5.6-sol xhigh · /workspace";

    assert_eq!(
        classify_composer(screen, expected),
        ComposerState::ExactSafe
    );
    assert_eq!(
        classify_receipt(screen, expected),
        ReceiptState::RetainedSafe
    );

    let human_draft = screen.replace("light-work receipt", "human draft");
    assert_eq!(
        classify_composer(&human_draft, expected),
        ComposerState::Changed
    );
    assert_eq!(
        classify_receipt(&human_draft, expected),
        ReceiptState::NotRetained
    );

    let unfamiliar_shape = screen.replace("\r\n  consistency", "\r\nconsistency");
    assert_eq!(
        classify_composer(&unfamiliar_shape, expected),
        ComposerState::Ambiguous
    );
    assert_eq!(
        classify_receipt(&unfamiliar_shape, expected),
        ReceiptState::Unproven
    );
}

#[test]
fn codex_hard_newline_shape_is_knowingly_admitted_until_issue_250() {
    let expected = "[DING] new st2 message: [id:7j8b6n] re: Q97 light-work receipt: VRS/source consistency complete (from dev3.compoundingtech.st2.message-sent.orchestration); check your inbox";
    // A buffer containing hard newlines plus two literal spaces renders identically to this
    // soft wrap. The screen-only ambiguity is accepted until https://github.com/compoundingtech/st2/issues/250.
    let screen = "\x1b[1m›\x1b[1C\x1b[0m[DING] new st2 message: [id:7j8b6n] re: Q97 light-work receipt: VRS/source\r\n  consistency complete (from dev3.compoundingtech.st2.message-\r\n  sent.orchestration); check your inbox\r\n\r\n\x1b[2C\x1b[0mgpt-5.6-sol xhigh · /workspace";

    assert_eq!(
        classify_composer(screen, expected),
        ComposerState::ExactSafe
    );
    assert_eq!(
        classify_receipt(screen, expected),
        ReceiptState::RetainedSafe
    );
}

#[test]
fn claude_word_wraps_preserve_short_rows_before_continuations() {
    let expected = "[DING] ? dev3.dotfiles.st2.claude-composer.worker: inspect the composer word wrapping behavior (from dev3.dotfiles.st2.main.orchestration); check your inbox [id:abc123]";
    let rule = claude_rule();
    let screen = format!(
        "Claude Code v2.1.220\r\n{rule}\r\n\
             ❯\u{00a0}[DING] ? dev3.dotfiles.st2.claude-composer.worker: inspect the\r\n  \
             composer word wrapping behavior (from\r\n  \
             dev3.dotfiles.st2.main.orchestration); check your inbox\r\n  \
             [id:abc123]\r\n{rule}\r\n\
             ⏵⏵ bypass permissions on (shift+tab to cycle)"
    );

    assert_eq!(
        classify_composer(&screen, expected),
        ComposerState::ExactSafe
    );
    assert_eq!(
        classify_receipt(&screen, expected),
        ReceiptState::RetainedSafe
    );

    let human_draft = screen.replace("composer word", "human draft");
    assert_eq!(
        classify_composer(&human_draft, expected),
        ComposerState::Changed
    );
    assert_eq!(
        classify_receipt(&human_draft, expected),
        ReceiptState::NotRetained
    );

    let unfamiliar_shape = screen.replace("\r\n  composer", "\r\ncomposer");
    assert_eq!(
        classify_composer(&unfamiliar_shape, expected),
        ComposerState::Ambiguous
    );
    assert_eq!(
        classify_receipt(&unfamiliar_shape, expected),
        ReceiptState::Unproven
    );
}

#[test]
fn preserved_clean_codex_context_footer_is_idle() {
    // Composer-to-end slice of clean zero-turn capture bb1eddba2725672287e4a73b0aaf50652250611566d9826977dc3c6639d70360.
    let screen = "\x1b[1m›\x1b[22m \x1b[2mImprove documentation in @filename\r\n\x1b[22m \r\n\x1b[0m\x1b[2X\x1b[2C\x1b[38;2;246;226;183mgpt-5.6-sol low\x1b[39;2m · \x1b[38;2;242;181;144;22mContext 0% used\x1b[45X\x1b[2A\x1b[33D\x1b[0m\x1b[?2004h\x1b[?1004h\x1b[?1049l\x1b[?1l\x1b[?7h\x1b[?6l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?25h\x1b[?2004l\x1b[4l\x1b[r\x1b[0m\x1b[0 q\x1b>\x1b(B\x1b[<99u\x1b[999;1H\n";

    assert_eq!(
        classify_composer(screen, "generic staged notice"),
        ComposerState::EmptySafe
    );
}

#[test]
fn preserved_post_turn_codex_frame_stays_ambiguous() {
    // Composer-to-end slice of post-turn capture f4abd2a65c008db1f52d62fa53f813dedde5c830632133c80a732dfbe12b95fe.
    let screen = "\x1b[48;2;30;30;30m \x1b[79X\r\n\x1b[1m›\x1b[22m \x1b[2mFind and fix a bug in @filename\x1b[47X\r\n\x1b[22m \x1b[79X\r\n\x1b[0m  \x1b[38;2;246;226;183mgpt-5.6-sol low\x1b[39;2mi· \x1b[38;2;242;181;144;22mContext 3% used\x1b[2A\x1b[33D\x1b[0m\x1b[?2004h\x1b[?1004h\x1b[?1049l\x1b[?1l\x1b[?7h\x1b[?6l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?25h\x1b[?2004l\x1b[4l\x1b[r\x1b[0m\x1b[0 q\x1b>\x1b(B\x1b[<99u\x1b[999;1H\n";

    assert_eq!(
        classify_composer(screen, "generic staged notice"),
        ComposerState::Ambiguous
    );
}

#[test]
fn strip_ansi_consumes_designate_g0_charset_sequence() {
    assert_eq!(
        composer::strip_ansi("Context 0% used\x1b>\x1b(B"),
        "Context 0% used"
    );
}

#[test]
fn startup_adopts_only_an_exact_recovery_or_backlog_composer() {
    let recovery = RECOVERY_POKE.to_string();
    let backlog = "[DING] ? cos: seeded [id:abc123]".to_string();
    let candidates = vec![recovery.clone(), backlog.clone()];
    assert_eq!(
        exact_staged_candidate(&staged_codex_screen(&backlog), &candidates),
        Some(backlog.clone())
    );
    assert_eq!(
        exact_staged_candidate(&staged_claude_screen(&recovery), &candidates),
        Some(recovery)
    );
    assert_eq!(
        exact_staged_candidate(&human_codex_screen(), &candidates),
        None
    );
}

#[test]
fn paste_then_two_exact_observations_precede_return() {
    use std::cell::RefCell;

    let text = "[DING] ? cos: ordered [id:abc123]";
    let screens = RefCell::new(VecDeque::from([
        idle_codex_screen(),
        staged_codex_screen(text),
        staged_codex_screen(text),
        accepted_codex_screen(text),
    ]));
    let actions = RefCell::new(Vec::new());
    let outcome = observed_poke_with_window(
        text,
        &mut || {
            actions.borrow_mut().push("peek");
            Ok(screens.borrow_mut().pop_front().unwrap())
        },
        &mut || {
            actions.borrow_mut().push("paste");
            Ok(())
        },
        &mut || {
            actions.borrow_mut().push("return");
            Ok(())
        },
        &mut || actions.borrow_mut().push("poll"),
        &mut || {
            actions.borrow_mut().push("receipt");
            Ok(())
        },
        Duration::from_millis(10),
    )
    .unwrap();
    assert_eq!(outcome, PokeOutcome::Delivered);
    assert_eq!(
        *actions.borrow(),
        ["peek", "paste", "peek", "peek", "receipt", "return", "peek"]
    );
}

#[test]
fn changed_modal_ambiguous_and_bounded_timeout_never_return() {
    use std::cell::RefCell;

    let text = "[DING] ? cos: guarded [id:abc123]";
    for screen in [
        human_codex_screen(),
        format!("Create a plan?\r\n{}", staged_codex_screen(text)),
        "unrecognized renderer".to_string(),
    ] {
        let actions = RefCell::new(Vec::new());
        let outcome = observed_poke_with_window(
            text,
            &mut || {
                actions.borrow_mut().push("peek");
                Ok(screen.clone())
            },
            &mut || {
                actions.borrow_mut().push("paste");
                Ok(())
            },
            &mut || {
                actions.borrow_mut().push("return");
                Ok(())
            },
            &mut || {},
            &mut || Ok(()),
            Duration::ZERO,
        )
        .unwrap();
        assert_ne!(outcome, PokeOutcome::Delivered);
        assert!(!actions.borrow().contains(&"return"));
        assert!(!actions.borrow().contains(&"paste"));
    }

    let screens = RefCell::new(VecDeque::from([idle_claude_screen(), idle_claude_screen()]));
    let actions = RefCell::new(Vec::new());
    let outcome = observed_poke_with_window(
        text,
        &mut || {
            actions.borrow_mut().push("peek");
            Ok(screens.borrow_mut().pop_front().unwrap())
        },
        &mut || {
            actions.borrow_mut().push("paste");
            Ok(())
        },
        &mut || {
            actions.borrow_mut().push("return");
            Ok(())
        },
        &mut || {},
        &mut || Ok(()),
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!(outcome, PokeOutcome::Staged);
    assert_eq!(*actions.borrow(), ["peek", "paste", "peek"]);
}

#[test]
fn final_observation_change_and_staged_retry_are_fail_closed() {
    use std::cell::RefCell;

    let text = "[DING] ? cos: final race [id:abc123]";
    let screens = RefCell::new(VecDeque::from([
        idle_codex_screen(),
        staged_codex_screen(text),
        human_codex_screen(),
    ]));
    let actions = RefCell::new(Vec::new());
    let outcome = observed_poke_with_window(
        text,
        &mut || Ok(screens.borrow_mut().pop_front().unwrap()),
        &mut || {
            actions.borrow_mut().push("paste");
            Ok(())
        },
        &mut || {
            actions.borrow_mut().push("return");
            Ok(())
        },
        &mut || {},
        &mut || Ok(()),
        Duration::from_millis(10),
    )
    .unwrap();
    assert_eq!(outcome, PokeOutcome::Staged);
    assert_eq!(*actions.borrow(), ["paste"]);

    let retry_screens = RefCell::new(VecDeque::from([
        staged_claude_screen(text),
        staged_claude_screen(text),
        format!("❯\u{00a0}{text}\r\n{}", idle_claude_screen()),
    ]));
    let retry_actions = RefCell::new(Vec::new());
    assert_eq!(
        retry_staged_with_window(
            text,
            &mut || Ok(retry_screens.borrow_mut().pop_front().unwrap()),
            &mut || {
                retry_actions.borrow_mut().push("return");
                Ok(())
            },
            &mut || {},
            &mut || Ok(()),
            Duration::ZERO,
        )
        .unwrap(),
        PokeOutcome::Delivered
    );
    assert_eq!(*retry_actions.borrow(), ["return"]);
}

#[test]
fn successful_transport_with_retained_or_unproven_pixels_is_not_delivered() {
    use std::cell::RefCell;

    let text = "[DING] ? cos: receipt truth [id:abc123]";
    for screen in [
        staged_codex_screen(text),
        idle_codex_screen(),
        human_codex_screen(),
        "unknown renderer".to_string(),
    ] {
        let actions = RefCell::new(Vec::new());
        let outcome = transport_and_observe_with_window(
            text,
            &mut || {
                actions.borrow_mut().push("transport");
                Ok(())
            },
            &mut || {
                actions.borrow_mut().push("peek");
                Ok(screen.clone())
            },
            &mut || actions.borrow_mut().push("poll"),
            &mut || {
                actions.borrow_mut().push("before-submit");
                Ok(())
            },
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(outcome, PokeOutcome::Staged);
        assert_eq!(
            *actions.borrow(),
            ["before-submit", "transport", "peek"],
            "transport success alone must never become Delivered"
        );
    }
}

#[test]
fn ambiguous_transport_receipt_and_retry_errors_retain_staged_ownership() {
    use std::cell::RefCell;

    let text = "[DING] ? cos: error truth [id:abc123]";

    let actions = RefCell::new(Vec::new());
    assert_eq!(
        transport_and_observe_with_window(
            text,
            &mut || {
                actions.borrow_mut().push("transport");
                anyhow::bail!("ambiguous transport")
            },
            &mut || {
                actions.borrow_mut().push("peek");
                Ok(idle_codex_screen())
            },
            &mut || {},
            &mut || {
                actions.borrow_mut().push("before-submit");
                Ok(())
            },
            Duration::ZERO,
        )
        .unwrap(),
        PokeOutcome::Staged
    );
    assert_eq!(*actions.borrow(), ["before-submit", "transport"]);

    assert_eq!(
        transport_and_observe_with_window(
            text,
            &mut || Ok(()),
            &mut || anyhow::bail!("unreadable receipt"),
            &mut || {},
            &mut || Ok(()),
            Duration::ZERO,
        )
        .unwrap(),
        PokeOutcome::Staged
    );

    let screens = RefCell::new(VecDeque::from([
        staged_codex_screen(text),
        staged_codex_screen(text),
    ]));
    let submits = RefCell::new(0);
    assert_eq!(
        retry_staged_with_window(
            text,
            &mut || Ok(screens.borrow_mut().pop_front().unwrap()),
            &mut || {
                *submits.borrow_mut() += 1;
                anyhow::bail!("ambiguous Return")
            },
            &mut || {},
            &mut || Ok(()),
            Duration::ZERO,
        )
        .unwrap(),
        PokeOutcome::Staged
    );
    assert_eq!(*submits.borrow(), 1);
}

#[test]
fn adapter_recognized_notice_with_an_empty_live_composer_is_a_positive_receipt() {
    let text = "[DING] ? cos: receipt truth [id:abc123]";
    assert_eq!(
        classify_receipt(&queued_codex_screen(text), text),
        ReceiptState::Accepted
    );
    assert_eq!(
        classify_receipt(&accepted_codex_screen(text), text),
        ReceiptState::Accepted
    );
    assert_eq!(
        classify_receipt(&staged_codex_screen(text), text),
        ReceiptState::RetainedSafe
    );
    assert_eq!(
        classify_receipt(
            &format!("ordinary transcript: {text}\r\n{}", idle_codex_screen()),
            text
        ),
        ReceiptState::NotRetained,
        "notice text outside an adapter-recognized accepted pattern is not a receipt"
    );
    assert_eq!(
        classify_receipt(
            &format!("old receipt: {text}\r\n{}", human_codex_screen()),
            text
        ),
        ReceiptState::NotRetained,
        "a parsed changed live composer positively excludes the exact owned notice"
    );
    assert_eq!(
        classify_receipt(&idle_codex_screen(), text),
        ReceiptState::NotRetained
    );
    assert_eq!(
        classify_receipt("unknown renderer", text),
        ReceiptState::Unproven,
        "an unrecognized screen cannot prove that the owned notice disappeared"
    );

    assert_eq!(
        classify_receipt(
            &format!("❯\u{00a0}{text}\r\n{}", idle_claude_screen()),
            text
        ),
        ReceiptState::Accepted
    );
    assert_eq!(
        classify_receipt(&staged_claude_screen(text), text),
        ReceiptState::RetainedSafe
    );
    assert_eq!(
        classify_receipt(
            &format!("ordinary transcript: {text}\r\n{}", idle_claude_screen()),
            text
        ),
        ReceiptState::NotRetained
    );
    assert_eq!(
        classify_receipt(&idle_claude_screen(), text),
        ReceiptState::NotRetained
    );
}

#[test]
fn soft_wrap_proofs_accept_short_known_continuations() {
    let text = "[DING] ? cos: receipt truth [id:abc123]";
    let (first, continuation) = text.split_at(32);
    let codex = format!(
        "\x1b[1m›\x1b[1C\x1b[0m{first}\r\n  {continuation}\r\n\r\n\
             \x1b[2C\x1b[0mgpt-5.6-sol xhigh · /workspace"
    );
    let rule = claude_rule();
    let claude = format!(
        "Claude Code v2.1.220\r\n{rule}\r\n❯\u{00a0}{first}\r\n  {continuation}\r\n\
             {rule}\r\n⏵⏵ bypass permissions on (shift+tab to cycle)"
    );

    assert_eq!(
        (
            classify_receipt(&codex, text),
            classify_receipt(&claude, text),
        ),
        (ReceiptState::RetainedSafe, ReceiptState::RetainedSafe),
        "both maintained composers prove short rows with their two-cell continuation indent"
    );
    assert_eq!(
        classify_receipt(&human_codex_screen(), text),
        ReceiptState::NotRetained
    );
    assert_eq!(
        classify_receipt(&staged_claude_screen("a changed human composer"), text),
        ReceiptState::NotRetained
    );
}

#[test]
fn staged_retry_submits_only_retained_safe_and_requires_a_receipt() {
    use std::cell::RefCell;

    let text = "[DING] ? cos: retry truth [id:abc123]";

    let retained = RefCell::new(VecDeque::from([
        staged_codex_screen(text),
        staged_codex_screen(text),
        staged_codex_screen(text),
    ]));
    let submits = RefCell::new(0);
    let outcome = retry_staged_with_window(
        text,
        &mut || Ok(retained.borrow_mut().pop_front().unwrap()),
        &mut || {
            *submits.borrow_mut() += 1;
            Ok(())
        },
        &mut || {},
        &mut || Ok(()),
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!(outcome, PokeOutcome::Staged);
    assert_eq!(*submits.borrow(), 1);

    for (screen, expected) in [
        (
            format!("Create a plan?\r\n{}", staged_codex_screen(text)),
            PokeOutcome::Staged,
        ),
        (idle_codex_screen(), PokeOutcome::NotRetained),
        (human_codex_screen(), PokeOutcome::NotRetained),
        ("unknown renderer".to_string(), PokeOutcome::Staged),
    ] {
        let submits = RefCell::new(0);
        let outcome = retry_staged_with_window(
            text,
            &mut || Ok(screen.clone()),
            &mut || {
                *submits.borrow_mut() += 1;
                Ok(())
            },
            &mut || {},
            &mut || Ok(()),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(outcome, expected);
        assert_eq!(
            *submits.borrow(),
            0,
            "blocked or unproven receipts must not receive Return"
        );
    }

    let accepted = queued_codex_screen(text);
    let submits = RefCell::new(0);
    assert_eq!(
        retry_staged_with_window(
            text,
            &mut || Ok(accepted.clone()),
            &mut || {
                *submits.borrow_mut() += 1;
                Ok(())
            },
            &mut || {},
            &mut || Ok(()),
            Duration::ZERO,
        )
        .unwrap(),
        PokeOutcome::Delivered
    );
    assert_eq!(*submits.borrow(), 0);
}

#[test]
fn staged_retry_keeps_unproven_and_retained_blocked_owned() {
    use std::cell::RefCell;

    let text = "[DING] ? cos: retry truth [id:abc123]";
    for screen in [
        "unknown renderer".to_string(),
        format!("Create a plan?\r\n{}", staged_codex_screen(text)),
    ] {
        let submits = RefCell::new(0);
        assert_eq!(
            retry_staged_with_window(
                text,
                &mut || Ok(screen.clone()),
                &mut || {
                    *submits.borrow_mut() += 1;
                    Ok(())
                },
                &mut || {},
                &mut || Ok(()),
                Duration::ZERO,
            )
            .unwrap(),
            PokeOutcome::Staged
        );
        assert_eq!(*submits.borrow(), 0);
    }
}

#[test]
fn pty_commands_have_a_real_outer_timeout() {
    let started = Instant::now();
    let error = output_with_timeout(
        Command::new("sh").args(["-c", "sleep 2"]),
        Duration::from_millis(30),
    )
    .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn session_watch_has_startup_grace_debounce_and_live_reset() {
    let mut watch = SessionWatch::default();
    for _ in 0..10 {
        assert_eq!(watch.step(false), WatchStep::Poll);
    }
    assert_eq!(watch.step(true), WatchStep::Poll);
    assert_eq!(watch.step(false), WatchStep::Poll);
    assert_eq!(watch.step(false), WatchStep::Poll);
    assert_eq!(watch.step(true), WatchStep::Poll);
    assert_eq!(watch.step(false), WatchStep::Poll);
    assert_eq!(watch.step(false), WatchStep::Poll);
    assert_eq!(watch.step(false), WatchStep::Gone);
}

#[test]
fn new_arrivals_is_fifo_and_archive_receipts_prevent_reding() {
    let agent = tempfile::tempdir().unwrap();
    let inbox = inbox_dir(agent.path());
    let archive = archive_dir(agent.path());
    let first = send_to_inbox(&inbox, "alice", Some("first"), None, &[], "one").unwrap();
    std::thread::sleep(Duration::from_millis(2));
    let second = send_to_inbox(&inbox, "bob", Some("second"), None, &[], "two").unwrap();

    let mut seen = HashSet::new();
    let initial = new_arrivals(&inbox, &mut seen);
    assert_eq!(
        initial
            .iter()
            .map(|message| message.filename.as_str())
            .collect::<Vec<_>>(),
        [first.as_str(), second.as_str()]
    );
    assert!(new_arrivals(&inbox, &mut seen).is_empty());

    archive_msg(&inbox, &archive, &first).unwrap();
    std::fs::copy(archive.join(&first), inbox.join(&first)).unwrap();
    assert!(
        new_arrivals(&inbox, &mut seen).is_empty(),
        "an archive receipt must suppress a restored inbox copy"
    );

    std::thread::sleep(Duration::from_millis(2));
    let third = send_to_inbox(&inbox, "carol", Some("third"), None, &[], "three").unwrap();
    assert_eq!(
        new_arrivals(&inbox, &mut seen)
            .into_iter()
            .map(|message| message.filename)
            .collect::<Vec<_>>(),
        [third]
    );
}

#[test]
fn staged_ownership_survives_archive_and_never_repastes() {
    let agent = tempfile::tempdir().unwrap();
    let inbox = inbox_dir(agent.path());
    let archive = archive_dir(agent.path());
    let filename = send_to_inbox(&inbox, "cos", Some("owned"), None, &[], "body").unwrap();
    let message = message::list_inbox(&inbox).unwrap().pop().unwrap();
    let expected = render_without_catalog(&message);
    let mut pending = VecDeque::from([PendingNotice::message(message)]);
    let poker = OwnershipPoker {
        pokes: Mutex::new(Vec::new()),
        retries: Mutex::new(Vec::new()),
        poke_outcomes: Mutex::new(VecDeque::from([PokeOutcome::Staged])),
        retry_outcomes: Mutex::new(VecDeque::from([
            PokeOutcome::Staged,
            PokeOutcome::Delivered,
        ])),
    };

    flush_without_catalog(None, &mut pending, &poker);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].staged_text(), Some(expected.as_str()));

    archive_msg(&inbox, &archive, &filename).unwrap();
    prune_archived_pending(&inbox, &mut pending);
    assert_eq!(
        pending.len(),
        1,
        "an already-started paste remains inspection-owned across archive"
    );

    flush_without_catalog(None, &mut pending, &poker);
    assert_eq!(pending.len(), 1);
    flush_without_catalog(None, &mut pending, &poker);
    assert!(pending.is_empty());
    assert_eq!(poker.pokes.lock().unwrap().as_slice(), [expected.as_str()]);
    assert_eq!(
        poker.retries.lock().unwrap().as_slice(),
        [expected.as_str(), expected.as_str()]
    );
}

// -----------------------------------------------------------------------------------------
// SPIKE — producer-side event supersede against staged payload ownership.
//
// Supersede is the one genuinely new semantic in
// `docs/vrs/.experiments/2026-08-20-pipes-event-model-differentiation.md`, and it archives an
// inbox item *from outside DING*, possibly while DING already owns that item's payload in a
// composer. These two tests are the seam: if either fails, the differentiated event model's
// main earned semantic is unsafe and the design collapses back to unification.
// -----------------------------------------------------------------------------------------

/// A real catalog with one agent-owned stream.
fn event_catalog() -> (tempfile::TempDir, PathBuf) {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "hetz", "worker", None);
    crate::event::publish_owner_binding_for_test(catalog.path(), "hetz").unwrap();
    let declaration = catalog.path().join("hetz/worker/agent.kdl");
    let source = std::fs::read_to_string(&declaration).unwrap();
    std::fs::write(
        declaration,
        source.replacen("\n}\n", "\n  stream \"gh-ci\" {}\n}\n", 1),
    )
    .unwrap();
    let inbox = inbox_dir(&catalog.path().join("hetz").join("worker"));
    (catalog, inbox)
}

fn emit_ci(root: &Path, event_id: &str, supersede: bool) -> String {
    crate::event::emit(
        root,
        "hetz",
        "hetz.worker",
        "gh-ci",
        event_id,
        Some("pr-42"),
        Some(&format!("CI {event_id} on PR #42")),
        event_id,
        supersede,
    )
    .unwrap()
    .filename
}

fn flush_in(root: &Path, pending: &mut VecDeque<PendingNotice>, poker: &dyn Poker) {
    flush_pending(
        DingContext {
            catalog_root: root,
            this_host: "hetz",
            recipient: "hetz.worker",
        },
        None,
        pending,
        poker,
    );
}

/// The race: the producer supersedes event N *while DING owns N's staged payload*. The
/// existing ownership rules must carry it — N is pasted exactly once and never again, the
/// archived-and-not-retained head releases FIFO, and N+1 still delivers. Nothing about
/// `flush_pending` or `prune_archived_pending` changes to make this true.
#[test]
fn a_producer_supersede_of_a_staged_event_never_repastes_and_the_successor_delivers() {
    let (catalog, inbox) = event_catalog();
    let root = catalog.path();

    let failure = emit_ci(root, "failure", true);
    let mut seen = HashSet::new();
    let mut pending: VecDeque<PendingNotice> = new_arrivals(&inbox, &mut seen)
        .into_iter()
        .map(PendingNotice::message)
        .collect();
    let failure_text = pending[0].text(
        DingContext {
            catalog_root: root,
            this_host: "hetz",
            recipient: "hetz.worker",
        },
        &mut None,
    );
    assert!(
        failure_text.starts_with("[DING] » hetz.worker/gh-ci:"),
        "an event announces itself as a world-event: {failure_text}"
    );

    let poker = OwnershipPoker {
        pokes: Mutex::new(Vec::new()),
        retries: Mutex::new(Vec::new()),
        poke_outcomes: Mutex::new(VecDeque::from([
            // the successor, once ownership of the superseded head is released
            PokeOutcome::Delivered,
        ])),
        retry_outcomes: Mutex::new(VecDeque::from([PokeOutcome::NotRetained])),
    };
    // DING stages the failure notice and owns it.
    let stage_only = OwnershipPoker {
        pokes: Mutex::new(Vec::new()),
        retries: Mutex::new(Vec::new()),
        poke_outcomes: Mutex::new(VecDeque::from([PokeOutcome::Staged])),
        retry_outcomes: Mutex::new(VecDeque::new()),
    };
    flush_in(root, &mut pending, &stage_only);
    assert_eq!(pending[0].staged_text(), Some(failure_text.as_str()));

    // The producer now supersedes: `success` is materialized and `failure` is archived under
    // DING's feet.
    let success = emit_ci(root, "success", true);
    assert!(!inbox.join(&failure).exists(), "the head was retired");
    assert!(inbox.join(&success).exists(), "the successor is unread");

    pending.extend(
        new_arrivals(&inbox, &mut seen)
            .into_iter()
            .map(PendingNotice::message),
    );
    prune_archived_pending(&inbox, &mut pending);
    assert_eq!(
        pending.len(),
        2,
        "the staged-but-archived head keeps ownership; the successor queues behind it"
    );

    flush_in(root, &mut pending, &poker);

    assert!(pending.is_empty(), "FIFO drained, nothing stuck");
    let success_text = poker.pokes.lock().unwrap()[0].clone();
    assert!(
        success_text.contains("CI success on PR #42"),
        "{success_text}"
    );
    assert_eq!(
        stage_only.pokes.lock().unwrap().as_slice(),
        [failure_text.as_str()],
        "the superseded notice was pasted exactly once, ever"
    );
    assert_eq!(
        poker.pokes.lock().unwrap().len(),
        1,
        "and the only fresh paste after supersede is the successor"
    );
    assert_eq!(
        poker.retries.lock().unwrap().as_slice(),
        [failure_text.as_str()],
        "the superseded head was released by inspection only, never re-pasted"
    );
}

/// The pessimistic half of the same race: the adapter still sees the superseded notice in the
/// composer. Ownership is retained, FIFO stays blocked behind it, and the successor is *not*
/// pasted on top of a live payload. Supersede therefore cannot leak a second paste into a
/// composer that is still holding the first.
#[test]
fn a_superseded_but_still_retained_staged_event_keeps_ownership_without_repasting() {
    let (catalog, inbox) = event_catalog();
    let root = catalog.path();

    emit_ci(root, "failure", true);
    let mut seen = HashSet::new();
    let mut pending: VecDeque<PendingNotice> = new_arrivals(&inbox, &mut seen)
        .into_iter()
        .map(PendingNotice::message)
        .collect();
    let failure_text = pending[0].text(
        DingContext {
            catalog_root: root,
            this_host: "hetz",
            recipient: "hetz.worker",
        },
        &mut None,
    );

    let poker = OwnershipPoker {
        pokes: Mutex::new(Vec::new()),
        retries: Mutex::new(Vec::new()),
        poke_outcomes: Mutex::new(VecDeque::from([PokeOutcome::Staged])),
        retry_outcomes: Mutex::new(VecDeque::from([PokeOutcome::Staged])),
    };
    flush_in(root, &mut pending, &poker);

    emit_ci(root, "success", true);
    pending.extend(
        new_arrivals(&inbox, &mut seen)
            .into_iter()
            .map(PendingNotice::message),
    );
    prune_archived_pending(&inbox, &mut pending);
    flush_in(root, &mut pending, &poker);

    assert_eq!(pending.len(), 2, "later FIFO work remains blocked");
    assert_eq!(
        poker.pokes.lock().unwrap().as_slice(),
        [failure_text.as_str()],
        "the successor is never pasted on top of a retained payload"
    );
    assert_eq!(
        poker.retries.lock().unwrap().as_slice(),
        [failure_text.as_str()],
        "the retained superseded notice is retried by inspection only"
    );
}

/// Platforms that cannot hardlink through the open-file descriptor path (macOS fdescfs
/// rejects linkat(AT_SYMLINK_FOLLOW) on /dev/fd/N with EPERM) fall back to a byte-copy
/// archive receipt. The differentiated supersession semantics must hold unchanged on that
/// path, the receipt must carry exactly the validated bytes, and no staging file may leak
/// into the archive.
#[test]
#[cfg(debug_assertions)]
fn archive_copy_fallback_preserves_supersede_ownership_without_staging_leftovers() {
    crate::event::TEST_FORCE_ARCHIVE_RECEIPT_COPY.store(true, Ordering::Relaxed);
    struct ResetGuard;
    impl Drop for ResetGuard {
        fn drop(&mut self) {
            crate::event::TEST_FORCE_ARCHIVE_RECEIPT_COPY.store(false, Ordering::Relaxed);
        }
    }
    let _guard = ResetGuard;

    let (catalog, inbox) = event_catalog();
    let root = catalog.path();
    let archive = crate::message::archive_dir(&root.join("hetz").join("worker"));

    let failure_filename = emit_ci(root, "failure", true);
    let mut seen = HashSet::new();
    let mut pending: VecDeque<PendingNotice> = new_arrivals(&inbox, &mut seen)
        .into_iter()
        .map(PendingNotice::message)
        .collect();
    assert_eq!(pending.len(), 1);
    let failure_bytes =
        std::fs::read(inbox.join(&failure_filename)).expect("staged event bytes");
    let failure_text = pending[0].text(
        DingContext {
            catalog_root: root,
            this_host: "hetz",
            recipient: "hetz.worker",
        },
        &mut None,
    );

    let poker = OwnershipPoker {
        pokes: Mutex::new(Vec::new()),
        retries: Mutex::new(Vec::new()),
        poke_outcomes: Mutex::new(VecDeque::from([PokeOutcome::Staged])),
        retry_outcomes: Mutex::new(VecDeque::from([PokeOutcome::Staged])),
    };
    flush_in(root, &mut pending, &poker);

    emit_ci(root, "success", true);
    pending.extend(
        new_arrivals(&inbox, &mut seen)
            .into_iter()
            .map(PendingNotice::message),
    );
    prune_archived_pending(&inbox, &mut pending);
    flush_in(root, &mut pending, &poker);

    assert_eq!(pending.len(), 2, "later FIFO work remains blocked");
    assert_eq!(
        poker.pokes.lock().unwrap().as_slice(),
        [failure_text.as_str()],
        "the successor is never pasted on top of a retained payload"
    );
    assert!(!inbox.join(&failure_filename).exists(), "head was archived");
    let receipt = std::fs::read(archive.join(&failure_filename))
        .expect("byte-copy archive receipt exists");
    assert_eq!(
        receipt, failure_bytes,
        "the copy receipt carries exactly the validated bytes"
    );
    let staging_leftovers: Vec<_> = std::fs::read_dir(&archive)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".st2-archive-"))
        .collect();
    assert!(
        staging_leftovers.is_empty(),
        "no staging files survive in the archive: {staging_leftovers:?}"
    );
}

#[test]
fn archived_not_retained_releases_fifo_without_repasting_owned_notice() {
    let agent = tempfile::tempdir().unwrap();
    let inbox = inbox_dir(agent.path());
    let archive = archive_dir(agent.path());
    let first = send_to_inbox(&inbox, "alice", Some("first"), None, &[], "one").unwrap();
    std::thread::sleep(Duration::from_millis(2));
    let second = send_to_inbox(&inbox, "bob", Some("second"), None, &[], "two").unwrap();
    let mut pending: VecDeque<PendingNotice> = message::list_inbox(&inbox)
        .unwrap()
        .into_iter()
        .map(PendingNotice::message)
        .collect();
    let first_text = notice_text_without_catalog(&pending[0]);
    let second_text = notice_text_without_catalog(&pending[1]);
    let poker = OwnershipPoker {
        pokes: Mutex::new(Vec::new()),
        retries: Mutex::new(Vec::new()),
        poke_outcomes: Mutex::new(VecDeque::from([
            PokeOutcome::Staged,
            PokeOutcome::Delivered,
        ])),
        retry_outcomes: Mutex::new(VecDeque::from([PokeOutcome::NotRetained])),
    };

    flush_without_catalog(None, &mut pending, &poker);
    archive_msg(&inbox, &archive, &first).unwrap();
    prune_archived_pending(&inbox, &mut pending);
    flush_without_catalog(None, &mut pending, &poker);

    assert!(pending.is_empty());
    assert_eq!(
        poker.pokes.lock().unwrap().as_slice(),
        [first_text.as_str(), second_text.as_str()]
    );
    assert_eq!(poker.retries.lock().unwrap().as_slice(), [first_text]);
    assert!(!inbox.join(first).exists());
    assert!(inbox.join(second).exists());
}

#[test]
fn unread_not_retained_keeps_fifo_ownership_without_repasting() {
    let agent = tempfile::tempdir().unwrap();
    let inbox = inbox_dir(agent.path());
    send_to_inbox(&inbox, "alice", Some("first"), None, &[], "one").unwrap();
    std::thread::sleep(Duration::from_millis(2));
    send_to_inbox(&inbox, "bob", Some("second"), None, &[], "two").unwrap();
    let mut pending: VecDeque<PendingNotice> = message::list_inbox(&inbox)
        .unwrap()
        .into_iter()
        .map(PendingNotice::message)
        .collect();
    let first_text = notice_text_without_catalog(&pending[0]);
    let poker = OwnershipPoker {
        pokes: Mutex::new(Vec::new()),
        retries: Mutex::new(Vec::new()),
        poke_outcomes: Mutex::new(VecDeque::from([
            PokeOutcome::Staged,
            PokeOutcome::Delivered,
        ])),
        retry_outcomes: Mutex::new(VecDeque::from([PokeOutcome::NotRetained])),
    };

    flush_without_catalog(None, &mut pending, &poker);
    flush_without_catalog(None, &mut pending, &poker);

    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].staged_text(), Some(first_text.as_str()));
    assert_eq!(
        poker.pokes.lock().unwrap().as_slice(),
        [first_text.as_str()]
    );
    assert_eq!(poker.retries.lock().unwrap().as_slice(), [first_text]);
}

#[test]
fn pending_delivery_ignores_busy_but_respects_fresh_dnd_archive_and_retry() {
    let agent = tempfile::tempdir().unwrap();
    let inbox = inbox_dir(agent.path());
    let archive = archive_dir(agent.path());
    let status_path = status::status_path(agent.path());
    let first = send_to_inbox(&inbox, "alice", Some("first"), None, &[], "one").unwrap();
    std::thread::sleep(Duration::from_millis(2));
    let second = send_to_inbox(&inbox, "bob", Some("second"), None, &[], "two").unwrap();
    let mut pending: VecDeque<PendingNotice> = message::list_inbox(&inbox)
        .unwrap()
        .into_iter()
        .map(PendingNotice::message)
        .collect();
    let poker = RecordingPoker::live();

    archive_msg(&inbox, &archive, &first).unwrap();
    prune_archived_pending(&inbox, &mut pending);
    assert_eq!(
        pending
            .iter()
            .filter_map(|notice| match notice {
                PendingNotice::Message { message, .. } => Some(message.filename.as_str()),
                PendingNotice::Recovery { .. } | PendingNotice::Adopted { .. } => None,
            })
            .collect::<Vec<_>>(),
        [second.as_str()]
    );

    *poker.failures.lock().unwrap() = 1;
    status::set_state(&status_path, status::State::Busy).unwrap();
    flush_without_catalog(Some(&status_path), &mut pending, &poker);
    assert_eq!(pending.len(), 1, "a failed head remains queued");
    flush_without_catalog(Some(&status_path), &mut pending, &poker);
    assert!(pending.is_empty());

    let third = send_to_inbox(&inbox, "carol", Some("third"), None, &[], "three").unwrap();
    pending.extend(
        message::list_inbox(&inbox)
            .unwrap()
            .into_iter()
            .filter(|message| message.filename == third)
            .map(PendingNotice::message),
    );
    status::set_state(&status_path, status::State::Dnd).unwrap();
    flush_without_catalog(Some(&status_path), &mut pending, &poker);
    assert_eq!(pending.len(), 1, "fresh dnd suppresses delivery");

    let stale = std::time::SystemTime::now() - status::STATUS_STALE - Duration::from_secs(1);
    let stale_ms = stale
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    std::fs::write(&status_path, format!("dnd\nv1 {stale_ms}\n")).unwrap();
    flush_without_catalog(Some(&status_path), &mut pending, &poker);
    assert!(
        pending.is_empty(),
        "stale dnd reads unknown and no longer suppresses"
    );

    let calls = poker.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0], calls[1], "the failed FIFO head retries first");
    assert!(calls[0].contains("second"));
    assert!(calls[2].contains("third"));
}

#[test]
fn staged_and_recovery_delivery_do_not_traverse_the_catalog() {
    let catalog = tempfile::tempdir().unwrap();
    let context = DingContext {
        catalog_root: catalog.path(),
        this_host: "h",
        recipient: "h.recipient",
    };

    let mut staged =
        PendingNotice::message(msg("1785070000000-abc123.md", "h.sender", Some("staged")));
    staged.set_staged_text(Some("immutable staged notice".to_string()));
    let mut pending = VecDeque::from([staged]);
    let poker = OwnershipPoker {
        pokes: Mutex::new(Vec::new()),
        retries: Mutex::new(Vec::new()),
        poke_outcomes: Mutex::new(VecDeque::new()),
        retry_outcomes: Mutex::new(VecDeque::from([PokeOutcome::Delivered])),
    };
    crate::discovery::reset_discovery_walk_count_for_test();
    flush_pending(context, None, &mut pending, &poker);
    assert_eq!(
        crate::discovery::discovery_walk_count_for_test(),
        0,
        "immutable staged delivery must not depend on catalog traversal"
    );

    let mut pending = VecDeque::from([PendingNotice::Recovery {
        startup: HashSet::from(["1785070000000-def456.md".to_string()]),
        in_inbox: true,
        staged_text: None,
    }]);
    let poker = RecordingPoker::live();
    crate::discovery::reset_discovery_walk_count_for_test();
    flush_pending(context, None, &mut pending, &poker);
    assert_eq!(
        crate::discovery::discovery_walk_count_for_test(),
        0,
        "generic recovery delivery must not depend on catalog traversal"
    );
    assert_eq!(poker.calls.lock().unwrap().as_slice(), [RECOVERY_POKE]);
}

#[test]
fn one_pending_batch_traverses_the_catalog_once() {
    let catalog = tempfile::tempdir().unwrap();
    declare_agent(catalog.path(), "h", "recipient", None);
    let mut pending = (0..10)
        .map(|index| {
            PendingNotice::message(msg(
                &format!("17850700000{index:02}-abc{index:03}.md"),
                "h.sender",
                Some("batch"),
            ))
        })
        .collect::<VecDeque<_>>();
    let poker = RecordingPoker::live();

    crate::discovery::reset_discovery_walk_count_for_test();
    flush_pending(
        DingContext {
            catalog_root: catalog.path(),
            this_host: "h",
            recipient: "h.recipient",
        },
        None,
        &mut pending,
        &poker,
    );

    assert!(pending.is_empty());
    assert_eq!(poker.calls.lock().unwrap().len(), 10);
    assert_eq!(
        crate::discovery::discovery_walk_count_for_test(),
        1,
        "one pending batch must share one catalog observation"
    );
}

#[test]
fn startup_recovery_notice_retries_in_memory() {
    let poker = RecordingPoker::live();
    *poker.failures.lock().unwrap() = 1;
    let mut pending = VecDeque::from([PendingNotice::Recovery {
        startup: HashSet::from(["1785070000000-abc123.md".to_string()]),
        in_inbox: true,
        staged_text: None,
    }]);

    flush_without_catalog(None, &mut pending, &poker);
    assert_eq!(pending.len(), 1);
    flush_without_catalog(None, &mut pending, &poker);
    assert!(pending.is_empty());

    let calls = poker.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1], "the failed FIFO head retries first");
    assert_eq!(calls[0], RECOVERY_POKE);
}

#[test]
fn startup_backlog_gets_one_generic_recovery_then_new_arrivals_poke() {
    let agent = tempfile::tempdir().unwrap();
    let inbox = inbox_dir(agent.path());
    let status_path = status::status_path(agent.path());
    status::set_state(&status_path, status::State::Available).unwrap();
    send_to_inbox(&inbox, "old", Some("seeded"), None, &[], "old").unwrap();

    let poker = RecordingPoker::live();
    let stop = AtomicBool::new(false);
    let config = DingConfig {
        poll: Duration::from_millis(5),
        status_refresh: Duration::from_secs(60),
    };

    // Two separate barriers, each with its own generous budget. Sharing one 3s deadline
    // across both meant the first wait could spend it: the loop then stopped after a single
    // poke and the count assertion failed while naming neither barrier. The ceiling bounds a
    // loop that never ran at all, so a loaded host cannot turn it into a failure — and `stop`
    // is set on every path, because panicking here would leave `run_ding` looping forever.
    let mut polled = false;
    let mut poked_twice = false;
    std::thread::scope(|scope| {
        scope.spawn(|| {
            polled = await_ding_progress(|| poker.probes.load(Ordering::SeqCst) > 0);
            send_to_inbox(&inbox, "new", Some("post-start"), None, &[], "new").unwrap();
            poked_twice = await_ding_progress(|| poker.calls.lock().unwrap().len() >= 2);
            stop.store(true, Ordering::SeqCst);
        });

        run_ding(
            DingContext {
                catalog_root: agent.path(),
                this_host: "h",
                recipient: "h.recipient",
            },
            &inbox,
            Some(&status_path),
            &poker,
            &config,
            &stop,
        )
        .unwrap();
    });

    assert!(polled, "the ding loop never reached its first poll");
    assert!(poked_twice, "the ding loop never delivered both notices");
    let calls = poker.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], RECOVERY_POKE);
    assert!(calls[1].contains("post-start"));
    assert!(!calls.iter().any(|call| call.contains("seeded")));
}

#[test]
fn deferred_delivery_backoff_bounds_short_lived_pty_attempts() {
    let agent = tempfile::tempdir().unwrap();
    let inbox = inbox_dir(agent.path());
    let status_path = status::status_path(agent.path());
    status::set_state(&status_path, status::State::Available).unwrap();
    send_to_inbox(&inbox, "alice", Some("active"), None, &[], "active").unwrap();

    let poker = RecordingPoker::live();
    poker.defer.store(true, Ordering::SeqCst);
    let stop = AtomicBool::new(false);
    let config = DingConfig {
        poll: Duration::from_millis(20),
        status_refresh: Duration::from_secs(60),
    };

    // Synchronize on the loop's own progress rather than a wall-clock window. `session_alive`
    // runs exactly once per iteration, so `probes` counts polls directly: stop only once the
    // first poke landed AND the loop polled several more times, which is precisely when a
    // missing backoff would poke again. A fixed window instead ends wherever the host's
    // scheduler leaves it — on a loaded machine before the first poll, so the test observed zero
    // pokes and failed while asserting nothing about backoff.
    const POLLS_AFTER_FIRST_POKE: usize = 5;
    let barrier = Duration::from_secs(30);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + barrier;
            while poker.calls.lock().unwrap().is_empty() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            let polled = poker.probes.load(Ordering::SeqCst) + POLLS_AFTER_FIRST_POKE;
            while poker.probes.load(Ordering::SeqCst) < polled && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            stop.store(true, Ordering::SeqCst);
        });
        run_ding(
            DingContext {
                catalog_root: agent.path(),
                this_host: "h",
                recipient: "h.recipient",
            },
            &inbox,
            Some(&status_path),
            &poker,
            &config,
            &stop,
        )
        .unwrap();
    });

    // Without this the counter-test is vacuous: one poke across one poll proves no backoff.
    assert!(
        poker.probes.load(Ordering::SeqCst) > POLLS_AFTER_FIRST_POKE,
        "the loop did not poll again, so no backoff was exercised"
    );
    assert_eq!(
        poker.calls.lock().unwrap().len(),
        1,
        "an unsafe composer must not respawn pty peek/send children every inbox poll"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn idle_ding_does_not_spin_on_its_own_inbox_reads() {
    let agent = tempfile::tempdir().unwrap();
    let inbox = inbox_dir(agent.path());
    std::fs::create_dir_all(&inbox).unwrap();
    let status_path = status::status_path(agent.path());
    status::set_state(&status_path, status::State::Available).unwrap();

    let poker = RecordingPoker::live();
    let stop = AtomicBool::new(false);
    let config = DingConfig {
        poll: Duration::from_millis(20),
        status_refresh: Duration::from_secs(60),
    };
    let started = Instant::now();

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            stop.store(true, Ordering::SeqCst);
        });
        run_ding(
            DingContext {
                catalog_root: agent.path(),
                this_host: "h",
                recipient: "h.recipient",
            },
            &inbox,
            Some(&status_path),
            &poker,
            &config,
            &stop,
        )
        .unwrap();
    });

    assert!(started.elapsed() >= Duration::from_millis(140));
    assert!(
        poker.probes.load(Ordering::SeqCst) <= 20,
        "idle DING must sleep at its configured cadence"
    );
}

#[test]
fn session_liveness_probe_reports_only_positive_evidence() {
    use crate::harness_state::SessionLiveness;
    let tmp = tempfile::tempdir().unwrap();

    // An unreadable registry proves nothing: missing or garbled pidfiles downgrade nothing.
    assert_eq!(
        session_liveness_in(tmp.path(), "absent"),
        SessionLiveness::Indeterminate
    );
    std::fs::write(tmp.path().join("garbled.pid"), "not-a-pid\n").unwrap();
    assert_eq!(
        session_liveness_in(tmp.path(), "garbled"),
        SessionLiveness::Indeterminate
    );

    // Our own pid is positive evidence of life.
    std::fs::write(
        tmp.path().join("live.pid"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    assert_eq!(
        session_liveness_in(tmp.path(), "live"),
        SessionLiveness::Alive
    );

    // A reaped child is positive evidence of death (ESRCH).
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    std::fs::write(tmp.path().join("dead.pid"), format!("{pid}\n")).unwrap();
    assert_eq!(
        session_liveness_in(tmp.path(), "dead"),
        SessionLiveness::Dead
    );
}

/// A deferral is the one outcome that both delivers nothing and leaves nothing behind, so it
/// has to carry why. The two causes want different responses: a human drafting in a harness we
/// understand clears on its own, a pane no harness can locate never will.
#[test]
fn a_deferral_names_the_cause_that_produced_it() {
    let text = "[DING] ? cos: guarded [id:abc123]";
    for (screen, expected) in [
        (
            human_codex_screen(),
            DeferralReason::ComposerChanged { harness: "codex" },
        ),
        // Located, but an unrecognised footer proves nothing about the screen. This is a wait,
        // not a coverage gap, and must not be reported as an unlocatable pane.
        (
            idle_codex_screen_with_footer("Esc to interrupt"),
            DeferralReason::ComposerUnproven { harness: "codex" },
        ),
        (
            "unrecognized renderer".to_string(),
            DeferralReason::NoMaintainedComposer,
        ),
    ] {
        let outcome = observed_poke_with_window(
            text,
            &mut || Ok(screen.clone()),
            &mut || Ok(()),
            &mut || Ok(()),
            &mut || {},
            &mut || Ok(()),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(outcome, PokeOutcome::Deferred(expected));
    }
}

/// The deferral has to reach the watch loop, which is the only place that knows the recipient
/// and can decide whether it is worth saying.
#[test]
fn flush_reports_a_deferral_outward() {
    let catalog = tempfile::tempdir().unwrap();
    let context = DingContext {
        catalog_root: catalog.path(),
        this_host: "h",
        recipient: "h.recipient",
    };
    let notice = || {
        VecDeque::from([PendingNotice::message(msg(
            "1785070000000-abc123.md",
            "h.sender",
            Some("hello"),
        ))])
    };

    let poker = RecordingPoker::live();
    poker.defer.store(true, Ordering::SeqCst);
    assert_eq!(
        flush_pending(context, None, &mut notice(), &poker).deferred,
        Some(DeferralReason::NoMaintainedComposer)
    );

    // A delivery that lands has nothing to report.
    let poker = RecordingPoker::live();
    assert_eq!(
        flush_pending(context, None, &mut notice(), &poker).deferred,
        None
    );
}

/// A pane stuck in one verdict costs one line, not one per retry — but a changed verdict, and
/// the first deferral after delivery resumed, are both news.
#[test]
fn only_a_changed_deferral_verdict_is_worth_reporting() {
    let changed = DeferralReason::ComposerChanged { harness: "codex" };
    let unknown = DeferralReason::NoMaintainedComposer;
    let mut journal = DeferralJournal::default();

    assert!(journal.observe(Some(changed)));
    assert!(!journal.observe(Some(changed)));
    assert!(journal.observe(Some(unknown)));
    assert!(!journal.observe(Some(unknown)));
    assert!(!journal.observe(None));
    assert!(journal.observe(Some(unknown)));
}
