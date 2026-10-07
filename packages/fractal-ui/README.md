# Fractal UI

A small, independent UI kit for the Fractal web client, built on React Aria Components and StyleX, with Storybook as its workshop.

It has two layers:

- `.` (`kit.tsx`, `tokens.css`): token-driven component families. The workshop renders them in three switchable visual directions (**Folio**, **Relay**, **Orbit**), each with light and dark palettes and two densities.
- `./assistant-ui` and its subpaths: the dark-first application surfaces the web app composes, built on assistant-ui 0.15.25. They cover the transcript, composer, tool calls and previews, the sidebar agent row and status glyph, the thread header, resizable splits, resource cards, the work log, the sync line and the workbench layout model. Every surface is controlled: the host supplies data, clock and transport, and unknown values render as unknown.

## Run

```sh
nix develop .#web
pnpm install --frozen-lockfile  # repository root
pnpm --filter @smalltalk/fractal-ui storybook        # http://127.0.0.1:53705
pnpm --filter @smalltalk/fractal-ui typecheck
pnpm --filter @smalltalk/fractal-ui build-storybook  # packages/fractal-ui/storybook-static/
```

The package is an explicit member of the root pnpm workspace and uses its pinned toolchain, frozen lockfile and lifecycle-script suppression. `bash scripts/ci-fractal-web` runs its typecheck and the static Storybook build after the shared install.
The kit keeps TypeScript 6.0.3 and declares the Node types used by its Storybook configuration explicitly.

The dev server binds to localhost on a fixed port (53705). To view it from another machine, forward the port, for example `ssh -L 53705:localhost:53705 <host>`, and open http://localhost:53705.

Stories:

- **Fractal UI / Visual language**: one working context rendered in the chosen direction. It contains a thread list, a conversation with a tool call and a diff, a terminal pane, a command palette and component specimens. `Explore` exposes direction, scheme and density as canvas controls and Storybook args. The `FolioLight` … `OrbitDark` and `Compact` stories are fixed comparison entries.
- **Fractal UI / Component families**: every family on one canvas (`AllFamilies`, `FamiliesDark`), under the same three switches.

## Families

`kit.tsx` exports these families. Each is labeled, keyboard-reachable and token-driven:

| Family | Notes |
|---|---|
| Button | `primary`/`secondary`/`quiet`/`danger`; `sm`/`md`; square icon-only; disabled |
| Badge, Pill | Labeled semantic status tags; tone is never the only signal |
| StatusDot | `ready`/`building`/`queued`/`error`/`canceled`, always with text |
| Spinner | Labeled `role=status` |
| Progress | `ProgressBar` with label, value, `maxValue`, tone and size |
| CodeBlock | Filename, language chip, copy action |
| CommandMenu | Grouped items with description, keywords and shortcut; `Autocomplete` virtual focus keeps typing in the field while arrows and Enter drive the list; empty state |
| ContextCard | Preview on hover **and** keyboard focus after a 350 ms intent delay |
| Description, Entity | Metadata pairs; named actor chip |
| EmptyState, Note | Honest empty state with an action; severity notes, filled or outlined |
| Input | Controlled; slash hint; Escape clears; clear button |
| Kbd | Shortcut keys |
| Toggle, Checkbox | `Switch`; checkbox with checked, indeterminate and disabled states |
| Menu | `MenuTrigger` with popover, shortcut hints, separators |
| Modal | `Modal`: focus trap, Escape and scrim dismissal, explicit close |
| Table | Row headers, single replace-selection, compact, optional sticky header |
| Tabs | Controlled; `shouldForceMount` keeps hidden panels' state |
| Tooltip | Terse labels, configurable delay and placement, no close delay |
| Icons | `CheckIcon`, `CopyIcon`, `XCircleIcon`: original inline strokes, `currentColor` |

## Tokens

`tokens.css` defines semantic tokens per direction × scheme:

- Color: canvas, panel, recess, ink, muted, line, accent, on-accent, selection, good, warning, danger, added, removed.
- Type: sans, display, mono.
- Radius: control, panel.
- Motion: duration, curve.
- Density: spacing, control height, chrome size. Each direction applies its own pixel bias.

Every foreground/background pair used for text meets WCAG AA (≥ 4.5:1) in all six palettes. The minimum, 4.59:1, is in Relay dark. Under `prefers-reduced-motion`, durations are zero and spinner and pulse animations stop. Fonts come from system stacks only; no font or image assets are distributed.

## Clean-room note

This kit was written fresh from behavior-only requirements. No existing design-system source, CSS, tokens, fonts, icons, logos, screenshots or microcopy was imported or consulted. Every palette value, spacing bias, type stack, corner scale and motion curve was chosen independently for this kit.

Behavioral inspiration came only from public product documentation of coding-agent workflows. No source code, assets, branding or copy was reused:

- [Coding-agent app workflow introduction](https://openai.com/index/introducing-the-codex-app/): parallel threads, isolated worktrees, review before landing.
- [Zed agent panel](https://zed.dev/docs/ai/agent-panel): keyboard-first palette, per-change review, explicit execution state.

Per direction, the metaphor and the behavior it emphasizes are:

- **Folio**, an annotated working notebook: warm paper, plum annotations, serif headings, deliberate 180 ms easing. It emphasizes progressive disclosure of tool input and output, and an explicit review checkpoint.
- **Relay**, a dispatch desk: mineral surfaces, teal signals, squared 2 px geometry, compact rows, 90 ms linear motion. It emphasizes keyboard-reachable actions, discoverable commands and visible execution outcomes.
- **Orbit**, a navigation instrument: indigo layers, amber bearings, 14/20 px curves, roomier spacing, 240 ms settling motion. It emphasizes keeping context while moving between threads, review and execution, and text-labeled attention.

All displayed content is newly written synthetic data. The terminal is a read-only fixture, not an emulator. Nothing contacts a filesystem, network service, model or provider.

This note records provenance practice. It is not legal clearance or an independent originality certification.
