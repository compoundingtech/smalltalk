# Terminal tab input and replies

This is the **baseline before terminal input fixes**. It is measured through real PTYs,
including the packaged PTY session daemon, with the same `Ui`, crossterm decoder,
`NativeTerminal`, alacritty parser, and painter used by stui. The only invented part of the
UI is its graph data. No display server or real agent/person graph is involved.

[`copper_probe.py`](../crates/stui/tests/copper_probe.py) is an invented raw terminal program.
It emits requested mode changes and queries, logs every input byte as hex, and logs SIGWINCH
geometry. File control keeps test commands out of its input log.
[`terminal_tab_probe.py`](../crates/stui/tests/terminal_tab_probe.py) drives the outer PTY and
observes both the program log and a transparent tap of the real session socket. The 544-scenario
[byte matrix](../crates/stui/tests/fixtures/terminal-tab-baseline.json) records every scenario;
known failures in that matrix describe the baseline, rather than desired terminal behavior.
Future fixes should update the affected expectations and keep the scenario coverage.

## Wheel reproduction and cause

The wheel sends **no bytes** in the terminal tab, on either screen, in all the tested mouse
modes and encodings. Chawan, Helix, Vim, and less all show the same failure on an invented
200-line local document; the [real-program receipt](../crates/stui/tests/fixtures/terminal-tab-program-baseline.json)
records the modes and bytes. Keyboard scrolling changes each program's screen, so the programs
are alive and their input connections work.

The first fault is in the UI routing, before `NativeTerminal::wheel`: `draw_terminal` draws
the native terminal without registering a `FramePane`. `Ui::mouse` locates the target of a
wheel event in `frame.panes`; it finds no terminal and never calls `wheel`. Mouse button
reports take a different path through `terminal_mouse` and do arrive. Looking only at the
alternate-screen check in `wheel` misses this fault.

There are further faults behind that routing failure: `wheel` requires alternate screen
for mouse reporting, emits coordinates `1;1` and discards modifiers, and emits arrows on
alternate screen even with mode 1007 disabled. These are confirmed in source; the tab probe
currently cannot reach that code through wheel input. The fixes need to cover both layers.

| Real program | Modes observed | Wheel bytes / screen change | Keyboard bytes / screen change |
| --- | --- | --- | --- |
| Chawan 0.4.4 | Alternate screen, mouse 1002, SGR, bracketed paste | None / no | `j` / yes |
| Helix 25.07.1 | Alternate screen, mouse 1003, SGR, focus, bracketed paste | None / no | `CSI B` / yes |
| Vim 9.2.1001 (`-Nu NONE -n`, `set mouse=a`) | Alternate screen, application cursor, mouse 1002, SGR, focus, bracketed paste | None / no | `SS3 B` / yes |
| less 710 | Alternate screen, application cursor, no mouse | None / no | `SS3 B` / yes |

## What reaches the program

`CSI` means `ESC [`; `SS3` means `ESC O`. Mouse coordinates below are relative to the pane,
one-based. Keyboard modifiers use xterm's parameter `1 + Shift + 2×Alt + 4×Ctrl`.
The matrix exercises all eight combinations, with both normal and application cursor mode.

| Input / mode requested by child | Observed bytes or action | Gap / reason |
| --- | --- | --- |
| Wheel, main or alternate screen; no mouse, 1000, 1002, or 1003; legacy, 1005, 1006, or 1015; all modifiers; up/down | No bytes | Terminal absent from wheel hit rectangles; local history also does not scroll by wheel |
| Alternate scroll 1007 enabled/disabled, application cursor enabled/disabled | No wheel bytes in all combinations | Same routing fault; underlying `wheel` also ignores 1007 |
| Mouse 1000 clicks/releases, left/middle/right | Reports reach child | Does not request drags |
| Mouse 1002 drags | Button motion reports reach child | Hover remains absent |
| Mouse 1003 drags and hover | Button drags arrive; hover sends nothing | `mouse_bytes` has no `Moved` arm |
| Mouse without tracking mode | No reports; left drag selects locally | Selection is stui's |
| Mouse Alt/Ctrl and Alt+Ctrl | Modifier bits 8/16/24 reach clicks and drags | Wheel loses them behind routing fault |
| Mouse Shift and every combination containing Shift | No child reports; local selection | Explicit selection override in `terminal_mouse` |
| Pixel mouse 1016 (terminal-browser) | Click remains `CSI <0;5;3M` in cell coordinates | Daemon reports 1016 set, but stui has no pixel mouse encoder |
| SGR encoding 1006 | `CSI <button;x;y M` (press/drag), `CSI <button;x;y m` (release) | Correct pane-relative positions for buttons |
| Legacy mouse encoding | `ESC [ M`, code+32, x+32, y+32 | Coordinates clamp at 223 |
| UTF-8 mouse encoding 1005 | Legacy bytes even past column 223 | Mode tracked, but encoder ignores `UTF8_MOUSE`; large coordinate becomes `ff` |
| urxvt encoding 1015 | Legacy mouse bytes | alacritty mode parser/encoder do not implement 1015; daemon can nevertheless report it set |
| Arrows, Home/End without modifiers | `CSI A/B/C/D/H/F`; application mode uses `SS3` | Supported |
| Arrows, Home/End with modifiers | `CSI 1;modifier A/B/C/D/H/F` | Supported, including Ctrl+Alt+Shift |
| Insert/Delete, PageUp/PageDown | `CSI 2/3/5/6 ~`, modified form adds `;modifier` | Shift+PageUp/PageDown is owned by stui history |
| F1–F4 | `SS3 P/Q/R/S`; modified form `CSI 1;modifier P/Q/R/S` | Supported |
| F5–F12 | `CSI 15/17/18/19/20/21/23/24 ~`, with modifier when present | Supported |
| Text, Unicode, Shift text | UTF-8 text; character case preserved | Supported |
| Ctrl letter, Ctrl+Shift letter | C0 byte, e.g. Ctrl+A = `01` | Shift distinction lost in legacy encoding |
| Alt letter, Ctrl+Alt(+Shift) letter | ESC prefix, then text or C0 byte | Shift distinction lost for Ctrl combinations |
| Enter, Ctrl+Enter, Shift+Enter | `0d` in all three cases | Shift+Enter and Ctrl+Enter collapse even when the outer decoder distinguishes them |
| Alt+Enter | `1b 0d` | Supported |
| Tab, Shift+Tab | `09`, `CSI Z` | Supported while terminal focused |
| Backspace, Ctrl+Backspace, Alt+Backspace | `7f`, `08`, `1b 7f` | Legacy `08` decodes as Ctrl+H |
| Lone Escape, followed later by `a` | `1b`, then separately `61`; Escape arrives within one second | No extra inner escape delay; outer crossterm timing applies |
| Escape and `a` together | `1b 61` (Alt+A) | Legacy escape/Alt ambiguity remains |
| Numeric keypad, normal/application mode | Ordinary digits/operators and CR | Application keypad mode is not used by `key_bytes` |
| Enhanced keypad input | Digits/operators and CR, flattened to ordinary keys | Crossterm's keypad state is ignored by encoder |
| Kitty keyboard protocol (`CSI >31u`) | Shift+Enter still CR; Ctrl+Alt+Shift+A still ESC + `01` | Inner alacritty config disables kitty keyboard; encoder is always legacy |
| Kitty press / repeat / release events | Press becomes ordinary text; repeat and release disappear | `Ui::key` accepts only `KeyEventKind::Press`, and re-encodes it as legacy |
| modifyOtherKeys (`CSI >4;2m`) | Same legacy encoding | Mode is not stored or used by stui |
| Bracketed paste 2004 enabled | `CSI 200~`, payload, `CSI 201~` | LF and CRLF normalize to CR |
| Paste with 2004 disabled | Payload only | Same newline normalization |
| Focus 1004 enabled/disabled, incoming focus-in/out | No bytes in either state | Guard does not enable outer focus reports; event dispatch ignores focus events |
| Resize while child runs | Child receives SIGWINCH and pane rows/columns | Confirmed against `TIOCGWINSZ`, not just UI geometry |

## Queries and program output

The **session daemon**, rather than the outer terminal, answers the child's queries.
`Requests::send_event` drops alacritty's reply events to avoid duplicate replies. The baseline
uses the repository's pinned `pty` revision `ef0aaf9`. An older packaged revision `15ca74f`
was also measured: it gives the same wheel failure but does not answer the size queries.
That distinction matters when diagnosing a program that waits for a reply.

| Child request | Observed answer / output | Owner or remaining gap |
| --- | --- | --- |
| DA1 `CSI c` | `CSI ?62;22c` | Daemon; advertises ANSI color, not sixel |
| DA2 `CSI >c` | `CSI >0;382;0c` | Daemon |
| XTVERSION `CSI >0q` | `DCS >\|pty(0.8) ST` | Daemon identity, not outer terminal identity |
| DSR `CSI 5n` | `CSI 0n` | Daemon |
| Cursor position `CSI 6n` | `CSI 1;1R` after home | Daemon cursor position |
| Window pixels `CSI 14t` | `CSI 4;576;944t` for a 36×118 pane | Daemon estimates 16×8 cell pixels, not measured outer pixels |
| Cell pixels `CSI 16t` | `CSI 6;16;8t` | Same estimate |
| Window cells `CSI 18t` | `CSI 8;36;118t` | Actual child PTY geometry |
| Screen cells `CSI 19t` | No answer | Neither daemon nor stui supplies it |
| OSC 10 / 11 color query | `rgb:c0c0/c0c0/c0c0` / `rgb:0000/0000/0000`, terminated by ST | Daemon constants; not the outer palette |
| DECRQM mode queries | `CSI ?mode;1$y` when set, `;2$y` when reset | Daemon tracks 1, 66, 1000/1002/1003/1004/1005/1006/1007/1015/1016, 1049, 2004, 2026, 2031, 2048; 5522 returns unsupported (`;0$y`); may disagree with stui's implementation |
| Kitty keyboard query `CSI ?u` | `CSI ?0u` after reset; `CSI ?31u` after enabling flags 31 | Daemon advertises flags stui's legacy input encoder does not honor |
| modifyOtherKeys query `CSI ?4m` | No answer even after setting level 2 | No query-reply path |
| OSC 52 clipboard write | Requested text decodes to `copper` | Native request reaches stui; live UI copies it through outer OSC 52 |
| OSC 52 clipboard read | No answer | Intentional refusal to read the person's clipboard |
| OSC 8 hyperlink | Link text appears | URL metadata is not painted or made clickable by native painter |
| OSC 0 title | Title appears in terminal/tab header | Used as the probe's output acknowledgment |
| Cursor shapes (`CSI Ps SP q`), hide/show | Block, underline, bar and blinking variants recognized; hidden cursor omitted | Native painter returns requested cursor style to live UI |
| Bell | Bell request recognized | Live UI flashes it when looking elsewhere |
| Synchronized output 2026 | Text appears after end-sync | Parser buffers synchronized output, with timeout recovery |
| Kitty graphics query | `APC Gi=31;OK ST` | **False end-to-end advertisement:** daemon accepts graphics, but stui does not display them |
| Kitty image transmission/display | No graphics bytes reach outer PTY | Native parser/painter retains text cells only |
| Sixel XTSMGRAPHICS query | No answer | No sixel negotiation path |
| Sixel image output | No sixel bytes reach outer PTY | No native image decoding/painting path |

A color-query reply did not appear as stray visible text in these isolated runs. It reaches
the raw probe as input, which is the correct direction. This does not reproduce or explain
all possible attach/echo configurations; forwarding queries to the outer terminal would
risk a second reply because the daemon already answers them.

### Images: what is missing

Conversation thumbnails use `Picker::from_query_stdio` and ratatui-image, but that picker is
not connected to a native terminal program's image stream. Passing APC/DCS bytes straight
through would place images in outer-screen coordinates and leave them behind after pane
movement or tab changes. A complete bridge needs access to decoded image data and placement
state, pane-relative positioning and clipping, and a resource lifecycle that moves, hides,
and deletes placements on resize, scroll, detach, and tab switches. It also needs to control
the daemon's capability replies so the child only learns a protocol the complete tab path
can support. Today neither kitty nor sixel support should be claimed for terminal tabs.

## terminal-browser reproduction

The official Linux **terminal-browser 0.13.4** bundle ran against the same real terminal tab,
with Electron's headless Ozone backend, an invented local HTML document, isolated storage,
and no display connection. `TERM=xterm-kitty` selected its kitty renderer; this does not
emulate or launch kitty. The [receipt](../crates/stui/tests/fixtures/terminal-browser-baseline.json)
records actual output: 71 graphics APCs from the live app, **zero** at the outer PTY, no wheel
input, and Ctrl+Alt+Shift+A arriving as legacy `ESC 01`. Frame counts vary with run duration.

The app enabled 1003, 1006, 1016, 1004, 2004, 2048, 2031, synchronized output, and kitty flags
1 then 27. Its [release source](https://github.com/zenbu-labs/terminal-browser/blob/v0.13.4/pixel/engine/crates/pixel-core/src/terminal.rs)
also probes keyboard support, pixel mouse, extended clipboard 5522, graphics transport,
cell size, and colors. The session daemon consumes query bytes when replying, so the
transparent output tap's request list contains the mode changes that it passes onward.

| terminal-browser requirement | Gap confirmed by the byte matrix / real app |
| --- | --- |
| Kitty image frames | App emits frames, but native tab drops every frame before outer rendering |
| Pixel mouse 1016 | Daemon claims it; clicks still use cells, wheel is lost |
| Kitty keyboard flags 27, repeat/release | Legacy encoding loses modifiers; repeat/release disappear |
| Focus 1004 | Focus events are not forwarded |
| Clipboard 5522 | DECRQM reports unsupported |
| Colors and color-change mode 2031 | Fixed daemon colors; no outer palette or color-change event path |
| In-band resize 2048 | Daemon claims mode set; ordinary SIGWINCH is verified, an in-band pixel report is not |

Use the optional [`terminal_browser_probe.py`](../crates/stui/tests/terminal_browser_probe.py)
with an already installed Linux bundle. It starts the daemon directly to avoid CLI host
AppArmor setup, forces headless operation, and closes only its own temporary session:

```sh
python3 crates/stui/tests/terminal_browser_probe.py --worker TEST_EXECUTABLE \
  --app /path/to/terminal-browser-bundle --record /tmp/browser-probe.json
```

The download was checked against the release SHA-256. On a Nix host the downloaded Electron
needed a compatible ELF interpreter/RPATH and libstdc++/libgbm; `--library-path` supplies
those app libraries without changing the worker's environment. This optional check is not a
CI dependency; the CI byte matrix independently covers these protocol requests.

## Selection and mode recovery

| Situation | Current behavior | Remaining gap |
| --- | --- | --- |
| Child has no mouse tracking | Left drag selects and copies through stui | Available |
| Child has mouse tracking, Shift+drag reaches stui | Local selection overrides reporting | Depends on the outer terminal delivering Shift |
| Kitty with its default mouse mapping | Kitty keeps Shift+drag for its own selection | stui cannot receive that gesture |
| Alt/Option+drag while child has mouse tracking | Forwarded as an Alt-modified mouse report | No dependable local-selection override |
| Selection-toggle chord and footer hint | Absent | A stui-owned toggle and hint are needed |
| Recover modes after a crashed child | No user-facing terminal-mode reset | Need to reset parser/daemon mouse, keyboard and paste state without removing detach |

Kitty documents that Shift selects in the outer terminal even when an application has
requested mouse reporting ([overview](https://sw.kovidgoyal.net/kitty/overview/),
[mouse configuration](https://sw.kovidgoyal.net/kitty/conf/#mouse-actions)). The PTY matrix
can inject Shift and verify stui's override, but cannot prove that a terminal application
will deliver it. The follow-on fix needs Alt/Option selection, an explicit selection toggle,
and mode recovery; this baseline does not claim those exist.

## Keys stui owns

Ctrl+\\ always detaches and is never sent to the child. Crossterm reports legacy `1c` as
Ctrl+4; that alias is also the exit route. Shift+PageUp/PageDown scroll stui's terminal
history. Shift+mouse selects/copies even while the child requested tracking. In an agent
terminal, the first Ctrl+C or Ctrl+D arms a two-second confirmation; the second sends it.
A shell terminal receives those two chords immediately.

The space controls are Ctrl+K (palette), Ctrl+Q (quit), Ctrl+S (sidebar), Ctrl+H (Home),
Ctrl+T/V/X/W (tabs/splits), Ctrl+O (zoom), Tab/Shift+Tab, and Alt+arrows. **In the current
focused terminal path these are passed to the child**, because `glass_key` returns before
handling space shortcuts. Their usual space actions apply after detaching. The byte matrix
records this existing exception explicitly; it should not be mistaken for proof that a
future input encoder may take over stui's navigation or remove the unconditional exit.

## Reproduce

The normal CI test requires the real `pty` package and Python 3. Both are supplied by the
repository's CI environment. Run outside an agent seat environment:

```sh
cargo nextest run -p stui --bin stui -E 'test(terminal_tab_probe_baseline)'
```

The ignored `terminal_tab_probe_worker` is a subprocess worker, not a skipped coverage test.
The active baseline test launches it with a harness-owned outer PTY. To inspect new results,
use that test executable's path from `cargo nextest list --message-format json`:

```sh
python3 crates/stui/tests/terminal_tab_probe.py --worker TEST_EXECUTABLE --record /tmp/probe.json
python3 crates/stui/tests/terminal_tab_probe.py --worker TEST_EXECUTABLE --check \
  --program cha=/path/to/cha --program hx=/path/to/hx \
  --program vim=/path/to/vim --program less=/path/to/less
```

The real-app checks use disposable config/cache/state directories and invented local files.
They do not require fetching a website. Review baseline changes as byte-level behavior changes;
do not regenerate a baseline simply to silence an unexpected result.
