# Terminal tab input, replies and images

Terminal tabs use the real PTY session daemon for durable terminal state and query
replies. stui renders text and history with alacritty_terminal, and uses the pinned
PTY engine for negotiated keyboard/mouse encoding and decoded image placements.
The outer terminal owns its own input decoding and clipboard. Child queries are
answered once for the pane; they are not forwarded to the outer screen.

The regression test runs the production Ui, crossterm decoder, attachment and
painter in a real outer PTY, with an invented raw program in a second real PTY.
[`copper_probe.py`](../crates/stui/tests/copper_probe.py) logs every input byte and
SIGWINCH. [`terminal_tab_probe.py`](../crates/stui/tests/terminal_tab_probe.py)
records the [current matrix](../crates/stui/tests/fixtures/terminal-tab-current.json).
The [original matrix](../crates/stui/tests/fixtures/terminal-tab-baseline.json) and
[program receipts](../crates/stui/tests/fixtures/terminal-tab-program-baseline.json)
retain the reproduction before fixes.

## Wheel and mouse

The original wheel failure occurred before the encoder: `draw_terminal` did not
register a `FramePane`, so UI wheel routing found no terminal. The fixed pane is
registered. Mouse reporting now works on either screen with actual pane-relative
coordinates and modifiers. Mode 1003 button/wheel codes also correct the underlying
PTY encoder's inappropriate motion bit.

| Request or input | Terminal tab behavior | Limit or ownership |
| --- | --- | --- |
| Wheel with mouse 1000/1002/1003 | Three wheel reports, on main or alternate screen | Alt/Option, Shift and selection mode hold it locally |
| Wheel without mouse, alternate screen and 1007 enabled | Three arrows; application cursor mode uses SS3 | Disabling 1007 stops this translation |
| Wheel otherwise | Scrolls stui history | There is no horizontal history axis |
| Click/release, drag, hover | Respect tracking mode; hover only under 1003 | The UI keeps selection overrides |
| SGR 1006 | CSI <button;x;y M/m with pane cells | Modifiers preserved when program owns mouse |
| Legacy encoding | X10 byte reports within representable coordinates | Out-of-range positions omitted; use 1006 |
| UTF-8 1005, urxvt 1015 | Negotiated UTF-8 / decimal reports | Input comes through the outer terminal's decoder |
| Pixel mouse 1016 | Coordinates at the cell center, scaled by cell metrics | Measured font metrics with kitty; otherwise 8×16 estimate |
| Resize | Kernel PTY size and SIGWINCH match pane | 2048 additionally emits an in-band resize report |

The optional real-program checks use disposable configuration and a local 200-line
document. Chawan 0.4.4, Helix 25.07.1, Vim 9.2.1001 and less 710 receive wheel input
and change their screen. Chawan/Helix/Vim receive mouse wheel reports; less receives
application-cursor down arrows. No display server, window or person's graph is used.
The [current receipts](../crates/stui/tests/fixtures/terminal-tab-program-current.json)
record the wheel and keyboard bytes.

## Keyboard, paste, focus and selection

`CSI` means ESC [; `SS3` means ESC O. The child's modes determine the encoding,
independently of the outer terminal's modes.

| Input or mode | Behavior | Limit or ownership |
| --- | --- | --- |
| Arrows, Home/End, PageUp/PageDown, Insert/Delete, F1–F12 | Normal/application cursor forms and modifier combinations | Shift+PageUp/PageDown scroll local history |
| Text, Unicode, Ctrl/Alt/Shift combinations | Legacy bytes or enhanced encoding as negotiated | Legacy terminals cannot distinguish every chord |
| Enter variants, Tab/Shift+Tab, Backspace | Modifiers preserved when the selected protocol can express them | Plain Tab remains program input |
| Escape alone / Escape then text | Outer decoder's timing; no added inner delay | Legacy Escape+letter remains indistinguishable from Alt+letter |
| Kitty keyboard | Flags, press/repeat/release and associated text honored | Original physical/layout alternate key data is unavailable after crossterm decoding |
| modifyOtherKeys | Engine encodes requested level; state query answered | Negotiation is separate from kitty keyboard |
| Keypad | Keypad identity retained when the outer terminal reports it | Ordinary digit bytes cannot identify a physical keypad |
| Bracketed paste 2004 | Adds delimiters only when requested | LF/CRLF normalize to CR, as before |
| Focus 1004 | Outer focus in/out forwarded when requested | Palette and tab focus transitions also update the program |
| Drag without program mouse | Selects and copies through OSC 52 | Actual clipboard acceptance depends on outer terminal settings |
| Alt/Option+drag | Selects locally even with program mouse | Works when kitty keeps Shift for itself |
| Shift+drag | Local selection when delivered to stui | Kitty's default mapping keeps this gesture outside stui |
| Ctrl+Alt+S | Toggles selection mode, withholding mouse from child | Footer shows how to resume program mouse |
| Ctrl+Alt+R | Resets input modes and returns to normal screen, retaining normal text/history | Clears both kitty stacks, mouse, focus, paste, keypad and resize-reporting modes |
| Ctrl+Alt+click on OSC 8 link | Copies destination through OSC 52 | No outer-screen hyperlink coordinates or browser launch |

Recovery is output-side control of the daemon, rather than reset escape bytes typed
into the shell. It is shared with other attached clients and survives reconnect.
It requires the pinned PTY version; older daemons ignore the extension. The
[dependency decision](https://github.com/compoundingtech/pty/blob/main/docs/decisions/0016-embedded-surfaces-can-recover-input-modes.md)
records this boundary.

Ctrl+\ always detaches, including through the palette, and is never sent to the
child. Legacy Ctrl+4 is its crossterm alias. Ctrl+K (or Command+K) opens the palette.
The selection/reset chords and Shift+PageUp/PageDown stay stui's, including their
repeat/release events. In an agent terminal Ctrl+C/D require two presses within two
seconds; a shell receives them immediately. The existing focused-terminal exception
for other space controls stays: Ctrl+Q/S/H/T/V/X/W/O, plain Tab/Shift+Tab and Alt+arrows
reach the program; their space actions apply after detaching.

## Queries and program output

| Request | Reply or effect | Owner / limit |
| --- | --- | --- |
| DA1 / DA2 / XTVERSION | CSI ?62;22c / CSI >0;382;0c / pty(0.8) | Daemon identity; sixel is not advertised |
| DSR / cursor position | CSI 0n / pane-relative CSI row;col R | Daemon's cursor |
| Window pixels 14t / cell pixels 16t | Virtual window / cell pixel dimensions | Declared kitty font metrics, otherwise estimate |
| Window cells 18t / screen cells 19t | CSI 8;rows;cols t / CSI 9;rows;cols t | Virtual pane, not outer monitor |
| DECRQM | Set/reset state; unsupported modes return status 0 | 5522 extended clipboard is unsupported |
| Kitty keyboard query / modifyOtherKeys query | Active flags / current level | Daemon; no second client answer |
| OSC 10/11 and indexed color queries | Fixed virtual palette replies, ST terminated | Default foreground c0c0, background 0000; no outer palette query |
| OSC 52 write | Copies decoded text through outer OSC 52 | No clipboard reads |
| OSC 52 read | Empty reply | Refusal remains bounded; program does not wait indefinitely |
| OSC 8 / OSC 0 | Link metadata retained / title updates tab header | Link destination can be copied locally |
| Cursor shape / visibility | Block, underline, bar and blinking forms | Uses the outer cursor only on an unobscured focused pane |
| Bell | Recorded; live UI flashes when unfocused | No host audio |
| Synchronized output 2026 | Buffered until end-sync or parser timeout | Recovery also clears it |
| Kitty graphics query | Accepted inline transmission reports OK | Pixel rendering in kitty; cell fallback elsewhere |
| File/shared-memory graphics query | Refused | Child falls back to portable inline data |
| Sixel / XTSMGRAPHICS | Unsupported, no sixel advertisement | See remaining gaps |

The isolated color probe produces no stray visible OSC 11 reply. Replies go to
child input. Forwarding the same query to the outer terminal would introduce a
second answer with different geometry or defaults.

## Images and terminal-browser

Inline kitty images are decoded by the same PTY engine used for durable replay.
stui reads owned image bytes and resolved placements, clips them to the terminal
pane, applies source crops and cell/pixel offsets, and remaps child ids to ids it
owns. Both direct and Unicode-placeholder placements render as outer kitty virtual
placements. Text-cell frame diffs move and clear the visible placeholders on
scroll, pane movement, resize, overlays and tab switches. Hidden/dropped panes and
replaced placements delete only the compositor's own stored image ids.
Child placeholders whose images have been deleted are blanked, even if their text
remains in history or reflows back into the pane; child image ids never reach the
outer terminal.

Transmission is independent of the first placeholder cell, so an overlay or a hole
cannot swallow it. Original placeholder colors mask holes and other images.
Negative-z images preserve nonblank text. On outer terminals without kitty, ordinary
halfblock cells provide a pane-safe fallback. The decoder storage is capped at
32 MiB; each image and the total visible composition are capped at 16 million pixels.

The official Linux terminal-browser 0.13.4 bundle was exercised with headless Ozone,
a local HTML document, isolated storage and no display connection. Its medium
probes are refused and it automatically falls back to inline compressed frames.
The fixed path emits image APCs into the outer PTY, forwards pixel mouse wheel
reports and preserves Ctrl+Alt+Shift+A as CSI 97;8u. Counts vary with frame duration.
The optional [runner](../crates/stui/tests/terminal_browser_probe.py) records the
actual bytes. Its [release source](https://github.com/zenbu-labs/terminal-browser/blob/v0.13.4/pixel/engine/crates/pixel-core/src/terminal.rs)
requests graphics, mouse 1003/1006/1016, focus, paste, enhanced keyboard, resize,
colors and extended clipboard; the byte matrix independently covers these requests.
The [current browser receipt](../crates/stui/tests/fixtures/terminal-browser-current.json)
shows both child and outer graphics. Chawan with `buffer.images=true` on
`https://new.space` also emits inline PNG frames that reach the outer PTY and produce
pane-contained image cells; its [image receipt](../crates/stui/tests/fixtures/terminal-tab-chawan-images.json)
records that check. Network content and frame counts can change.

## Remaining gaps

| Gap | Why it stays |
| --- | --- |
| Sixel decoding and capability negotiation | No sixel decoder in the pinned engine; not advertised. Kitty inline frames are the supported pixel path |
| File, temporary-file and shared-memory image media | Paths belong to the child host and cannot safely be read by a remote surface; inline transport and replay work across hosts |
| Animation commands and sophisticated negative-z background compositing | This compositor paints resolved static frames, replacement images and text; no animation timeline or per-cell background blending API |
| Extended clipboard 5522 / clipboard reads | Deliberately unsupported; reads return empty rather than accessing the person's clipboard |
| Physical key identity and layout alternate keys | Crossterm exposes logical keys and keypad state, not all original kitty fields; cannot recreate information the outer decoder discarded |
| Precise pointer pixels | Outer crossterm reports cells; cell-center coordinates are the available resolution |
| Dynamic outer palette and color-change notifications | The session owns fixed virtual colors; it cannot mirror every attached outer terminal's theme |
| Outer terminals that consume Option/Shift gestures or refuse OSC 52 | Selection toggle avoids gesture dependence; clipboard policy belongs to that terminal |

## Reproduce

Use the Nix development shell, whose PTY package and Python are CI inputs, and run
outside an agent seat environment:

```sh
cargo nextest run -p stui --bin stui -E 'test(terminal_tab_protocols)'
python3 crates/stui/tests/terminal_tab_probe.py --worker TEST_EXECUTABLE --record /tmp/tab.json
python3 crates/stui/tests/terminal_browser_probe.py --worker TEST_EXECUTABLE \
  --app /path/to/terminal-browser-bundle --record /tmp/browser.json
```

The ignored worker is launched by the active regression test with its own controlling
PTY. It is not skipped coverage. Optional real programs use `--program NAME=PATH`.
The browser runner starts its daemon directly, avoiding CLI host setup, and closes
only its temporary processes. The release download was SHA-256 checked; on Nix its
Electron binary needed an owned ELF interpreter/RPATH patch and optional library path.
