# stui pointer targets

This inventory covers both spaces and the classic layout. The source of truth is
[`Hit`](../crates/stui/src/ui/doc.rs), the rectangles painted into `FrameInfo.hits`,
[`Ui::click` / `Ui::mouse`](../crates/stui/src/ui/mod.rs), and the
[`context` resolver](../crates/stui/src/ui/context.rs). No client contract changes
are needed for pointer feedback.

## Feedback and precedence

Motion highlights the topmost visible target, without acting, selecting or
focusing. The [`hover` pass](../crates/stui/src/ui/hover.rs) brightens its existing
background; HTTP/markdown links and `PaneIntent::Open` get an underline instead.
Opaque menus, palettes, Home and popovers hide covered targets, including their
blank cells. Dragging and loss of window focus clear pointer feedback. A terminal
without motion support keeps the same click and keyboard behavior.

The footer says `Click: … · Right: …` while a target is hovered. The
[`clickable` pass](../crates/stui/src/ui/clickable.rs) obtains the right-hand label
from the **same resolver that opens the menu**, including the subject under a
scrolled conversation. Labels shorten to fit; both operations remain named. A
pending confirmation, flash message or offline diagnostic keeps its footer.
The build stays at the right edge. Away from targets the footer starts `Keys:`:
these are keyboard instructions, not buttons.

A first click in an unfocused split changes only focus, except its composer,
which focuses the split and the editor. The footer names this first-click behavior.
Right-click opens a menu without changing focus or selection. A menu owns its
whole press/drag/release gesture and wheel. Clicking a row chooses it; clicking
elsewhere or right-clicking closes it. Esc always closes. Destructive actions
still require `y`; Enter never confirms them. F10 opens top-bar actions, and
Shift+F10 / the Menu key opens the focused subject or split menu. Menu rows show
existing shortcuts, and arrows plus Enter reach actions without a pointer.

The right-click precedence in the tables below is:

1. A space tab gets its tab menu. Now opens from the top bar, not a fixed tab.
2. A conversation entry gets its message menu, even over a tool or link in it.
3. A list row gets its subject menu (agent, mission or Home item where supported).
4. A graph link/card gets the subject's menu.
5. A Home, mission or declaration document gets its subject menu.
6. A supported standalone bar/control gets its own action menu.
7. An uncovered space body or strip gets the split menu; other places have no menu.

Agent menus offer conversation, message actions, available terminal/details/find
and seat actions, and copy path. Mission menus offer open, declaration and copy
path. Home menus reuse that card's existing actions and chat. Message menus offer
copy exact text, copy id, and reply where supported. There is no unread action:
the API supplies message-read, not mark-unread. Split menus offer existing
new-tab/split actions; tab menus additionally offer show, move, split with that tab
and close where allowed. Palette rows do not open a right-click menu.

## Every Hit variant

“Context” below means the precedence above; the footer names the resolved menu or
`no menu`. “Background” means the shared hover highlight. Button wording comes
from the painted label, so a card's specific action stays visible in the footer.

| Hit | Visible target / left click | Right click | Hover |
| --- | --- | --- | --- |
| `Answer(index)` | Structured answer row chooses it; Enter sends | Home context | Background; choose answer / Enter sends |
| `Voice` | Composer microphone starts voice input | Context | Background; start voice input |
| `NewTerminal` | Home launcher control creates a shell tab | Own action / context | Background; new terminal |
| `SidebarRow(index)` | Sidebar row selects on press, opens on release; drag can place it in a split | Subject context | Background; open item in tab |
| `SidebarSection(index)` | Sidebar heading switches section and takes sidebar focus | Context | Background; show section |
| `Usage` | Space top-bar spend toggles Usage | Own action | Background; show/hide Usage |
| `Connection` | Connection glyph/word opens this machine when known and live, otherwise shows connection status | Own action | Background; connection details |
| `Tab(index)` | Classic section label, badge and demo tag switch section | Own action | Background; show section |
| `Row(index)` | Classic list / list pane selects row | Subject context where supported | Background; select row |
| `Message` | Visible conversation entry: left drag selects/copies text | Message menu | Background over visible entry; drag to select/copy |
| `Subject` | Home, mission or declaration text: left drag selects/copies text | Subject menu | Background over visible document; drag to select/copy |
| `Resize` | Split border: drag resizes, double-click equalizes | No menu on the divider | Background; resize / equalize |
| `Key(char)` | Card button or actions row invokes its printed key; classic logo toggles list with `s`; agent details uses `i` | Context | Background; printed action (logo: show/hide list) |
| `Enter` | Card send/create button submits its current answer, chat or form | Context | Background; printed action |
| `Escape` | Card cancel/back button stops editing, cancels form/decision or closes popover; popover backdrop closes | Context; backdrop normally none | Button background; backdrop is a dismissal gesture, no highlight |
| `ToggleTool(id)` | Mission step row toggles its expanded details | Mission context | Background; expand/collapse |
| `Pane(intent)` | Conversation control; intent breakdown below | Message context | Background except Open underline; intent's action |
| `JumpLatest` | New-lines badge resumes following latest | Context | Background; jump to latest / End |
| `Composer` | Message box focuses editing (`c`) | Context | Background; edit message |
| `Help` | Help overlay accepts any press to close | Any press closes help | No hover while help owns input; title explicitly says any key or click closes |
| `Open(subject)` | Opens subject; no direct painted registration today (used by menu dispatch) | Subject context if registered | Background / open subject if registered |
| `Peek(subject)` | Underlined graph reference opens card; already-open popover background swallows left click | Subject context | Background; show card, or card already open |
| `Actions(agent)` | Agent header's `⋯ actions` opens agent actions card | Agent context | Background; agent actions |
| `Field(index)` | Form value row focuses field | Context | Background; focus field / Tab |
| `Revoke(device)` | Device button asks to revoke; `y` confirms | Context | Background; ask to revoke |
| `Detach` | Terminal status row leaves terminal (`Ctrl+\\`) | Context | Background; leave terminal |
| `GlassMenu` | Space name opens spaces palette (`Ctrl+G`) | Own action | Background; spaces |
| `PaletteSection(index)` | Top-bar counts open that palette section | Own action | Background; agents / missions / fleet |
| `Home` | `⌂` and needs-you count toggle Now (`Ctrl+H`) | Own action | Background; show/hide Now |
| `Link(url)` | Raw HTTP(S) or remembered markdown link copies complete URL; wrapped parts copy the same address | Message/subject context where applicable, otherwise copy-link action | Underline; copy link |
| `Split(right)` | Home launcher controls split right/below (`Ctrl+V` / `Ctrl+X`) | Own action | Background; split right/below |
| `Menu(action)` | Context-menu row runs existing action | Closes menu | Background; painted row action |
| `GlassTab(group, tab)` | Shows tab; drag reorders, moves or splits; middle press and release over the same tab closes via the existing tab-menu action | Tab menu | Background; show tab / drag to move or split / middle-click close |
| `GlassAdd(group)` | `+` focuses group and opens new-tab palette (`Ctrl+T`) | Split context | Background; new tab |
| `PaletteChoice(index)` | Palette result opens it | No menu | Background; open painted result |

The shared conversation document's `PaneIntent` has five variants. `Expand`
toggles a folded tool/thinking block; `Image` reads and opens an image attachment.
Those are currently painted by the shared renderer. `Open` opens a subject,
`Send` sends its text, and `LoadOlder` requests earlier conversation entries; stui
handles these embedding intents but the current shared renderer registers no
visible control for those three. The shell's new-lines badge is `JumpLatest`,
not `LoadOlder`.

## Every production registration site

Paths below are under [`crates/stui/src/ui`](../crates/stui/src/ui). `Doc.targets`
is translated to visible rectangles after clipping and scrolling, rather than
making hidden rows clickable. Test-only synthetic hits are excluded.

| File / function | What it registers or transports |
| --- | --- |
| `mod.rs`: `hit` | Appends a rectangle and Hit to the frame; shared final sink |
| `mod.rs`: `links` | `Link` for raw HTTP(S) parts and remembered underlined markdown labels |
| `mod.rs`: `top_bar` | Classic logo `Key('s')`; every section/badge `Tab`; connection glyph/word `Connection` |
| `mod.rs`: `draw_list`, `draw_list_as` | Every visible data row `Row`; callback supplies `SidebarRow` in sidebar; headers/empty states excluded |
| `mod.rs`: `draw_agent` | Narrow details/back header and details rule button `Key('i')` |
| `mod.rs`: `draw_composer` | Input box `Composer`; available voice control `Voice` |
| `mod.rs`: `pane` | Clipped conversation entries `Message` (excluding history loading note); subject documents `Subject`; document targets; `links`; new-lines badge `JumpLatest` |
| `mod.rs`: `draw_popover` | Backdrop `Escape`, card interior `Peek(current subject)`, `links`, clipped document targets; overlay blocks underlying split focus/drag |
| `mod.rs`: `draw_terminal` | Native and legacy terminal status rows `Detach`; body is program-owned |
| `mod.rs`: `draw_help` | Full-area `Help` dismissal target, with input/hover override |
| `glass.rs`: `render_glass` | Each divider `Resize` alongside existing drag metadata |
| `glass.rs`: `draw_sidebar` | Section headings `SidebarSection`; invokes `draw_list_as` with `SidebarRow` |
| `glass.rs`: `launcher_bar` | Home launcher buttons `NewTerminal`, `Split(true/false)`; used by empty initial group's `draw_group` and `draw_home_popover` |
| `glass.rs`: `status_line` | `Connection`, `Home`, `PaletteSection(1/2/3)`, `Usage`, `GlassMenu` |
| `glass.rs`: `tab_strip` | Each painted tab `GlassTab`; `+` `GlassAdd` |
| `glass.rs`: `draw_palette` | Each visible result `PaletteChoice` |
| `glass.rs`: `draw_context_menu` | Each visible menu row `Menu` |
| `conversation.rs`: `Cache::render` | Converts shared conversation targets to `Pane(intent)` |
| `doc.rs`: `DocExt::buttons` | Registers each supplied button Hit over its printed key and label |
| `doc.rs`: `DocExt::card` | Offsets nested targets into card coordinates; does not invent targets for borders or titles |
| `screens.rs`: `link` | Underlined graph-reference label `Peek` |
| `screens.rs`: `text_box` | Home card chat/answer/feedback box `Composer` |
| `screens.rs`: `structured_request` | Answer row `Answer`; sends/cancels `Enter`/`Escape`; choose/write/dismiss `Key('a'/'c'/'x')` |
| `screens.rs`: `confirm_row` | Confirmation `Key('y')` and `Escape` |
| `screens.rs`: `home_detail` | Review, feedback, launch, revision, custom/structured/simple request, update and message card buttons (`Key`, `Enter`, `Escape`); related subjects through `link`; chat footer (`Key('t'/'g')`, `Enter`, `Escape`) and boxes through `text_box` |
| `screens.rs`: `agent_header` | `Actions(agent)` at the right of the identity row |
| `screens.rs`: `mission_detail` | Step `ToggleTool`; agents `Peek`; declaration `Key('k')`; uses `what_you_can_do` |
| `screens.rs`: `mission_kdl` | Back button `Key('k')` |
| `screens.rs`: `what_you_can_do` | Restart `Key('R')`, chat `Peek(agent)`, retry `Key('r')`, cancel `Key('X')` with existing confirmation handling |
| `screens.rs`: `fleet_detail` | Agent links `Peek` |
| `screens.rs`: `worktree_detail` | Agent and mission links `Peek` (invented demo data) |
| `screens.rs`: `agent_actions_doc` | Entire action rows `Key(action.key)` |
| `screens.rs`: `peek` | Agent go/message `Key('g'/'t')`; mission/Home go `Key('g')` |
| `screens.rs`: `agent_details` | Back-to-conversation `Key('i')` |
| `screens.rs`: `new_mission_form` | Each field `Field`; next/create/cancel `Key(Tab)`, `Enter`, `Escape` |
| `screens.rs`: `devices_card` | Per-device `Revoke` button |

The shared [`st3-conversation-ui`](../crates/st3-conversation-ui/src) renderer
adds `Expand` targets in tool/thinking blocks and `Image` targets for user/mail
attachments. Its document card/append helpers transport those targets; the stui
adapter and shell `pane` turn them into the final visible hit map. `usage.rs`
registers no card targets: selectable usage list rows come through `draw_list_as`.

## Gestures and things that are not buttons

| Place / appearance | Actual behavior and visible indication |
| --- | --- |
| Footer keys, build, demo notice | Keyboard/status text, no Hit; `Keys:` makes instruction role explicit |
| Classic host and person beside connection | Muted identity/status, no Hit; only the connection word is clickable |
| Top-bar border, card border/title, list section headings/counts, legend dots, meters | Decoration or data, no Hit/hover unless inside a larger row target |
| Agent harness/model/host/age, queue/progress text, mission facts, profile paths | Plain status/data; a declared graph link or URL within it remains a real target |
| Structured request subjects and update about/subjects | Plain dim references without the former `↗` link styling; HTTP(S) addresses still register real copy-link targets |
| Feedback source | HTTP(S) explicitly says `copy link` and is underlined; unsupported references say `reference` and are dim, without underline |
| Underlined graph references | Real `Peek` targets; different from URLs, which copy rather than browse |
| Zoomed split label `zoomed · ctrl+o` | Keyboard/status hint, no Hit; Ctrl+O toggles zoom |
| Plain document text / blank pane space | Left drag is text selection, not an action button; message and subject regions show hover/context hints, other text shows selection only while dragging |
| Scrollbars | Scroll position indicators; wheel/PageUp/PageDown scroll the pane, scrollbar track is not a button |
| Popover backdrop / palette or Home outside | Dismissal gestures, no button styling or hover; Esc provides the keyboard exit |
| Help | Full-screen dismissal gesture, title says any key or click closes; no hover behind it |
| Native terminal body | Program owns its mouse events, including motion and right-click when requested; stui adds no UI highlight or menu there. Alt/Shift drag or terminal selection mode selects text; Ctrl+\\ leaves; Ctrl+K opens palette; Ctrl+Alt+R resets modes |
| Legacy terminal body | Existing terminal/text-selection handling, no stui Hit; its status row remains Detach |

When adding a control, register its visible rectangle or document target, update
this inventory and the exhaustive footer match, and verify both pointer and
keyboard routes. Register broad selection/context regions before more precise
links and buttons so the latter win. Do not style a label like a link unless it
has a target. An overlay must own clicks as well as hide underlying hover.
