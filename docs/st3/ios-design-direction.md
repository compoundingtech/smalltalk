# iPhone design direction

The iPhone app follows system typography, appearance and navigation. The web supplies information structure: agent identity and status, conversations with disclosed work, and related resources. This direction for [#2784](https://github.com/compoundingtech/smalltalk/issues/2784) was confirmed on 2026-10-14. Merge it before rebuilding screens.

## Shared rules and surface ownership

| Boundary | Direction |
| --- | --- |
| Shared product rules | Keep generated contracts, stable IDs, action capabilities and server authority. Use pure [st3-views] models for Home grouping/counts, alert ownership, requests/answers and conversation normalization. Share semantic meanings and fixtures. |
| Web ownership | [packages/fractal-ui] owns DOM components, StyleX tokens, React Aria and pane geometry. [LiveAgentWorkspace] and [ConversationPane] own web navigation, data demand and kit adapters. |
| iPhone ownership | [App] and [navigation] own native stacks, tabs and sheets; [ui], [theme] and screens own text styles, dynamic colors, touch layout and keyboard behavior. Pairing, key storage, transport and notification integration remain platform responsibilities. |

Today the phone imports st3-views; the web adapts SDK data into the Fractal kit. Extract reusable pure rules into st3-views as slices need them; keep renderers separate. The kit's [sync-line] model is intended for st3-views but has not migrated.

## Typography and appearance

Replace fixed IBM Plex Mono 14/20 content, Mocha colors, forced DarkTheme and light status bar ([ui], [theme], [App]) with system text styles and semantic colors. Use Body for prose and row titles, Headline for emphasis and secondary styles for metadata. Support Dynamic Type through accessibility sizes with wrapping, expanding rows. Keep monospace for terminal cells, code and identifiers.

Follow device light/dark appearance throughout; the [web's manual scheme choice][LiveAgentWorkspace] stays on the web. Use blue for actions, orange for attention and red for faults and destructive actions, always with labels and symbols. Map shared tokens such as `person` by meaning. Use native materials and controls, comfortable touch targets and motion respecting Reduce Motion, following Apple's [typography](https://developer.apple.com/design/human-interface-guidelines/typography) and [color](https://developer.apple.com/design/human-interface-guidelines/color) guidance.

## Navigation and Home first

Keep **Now, Resources, Fleet, Spaces**, saved tab order and a stack per tab ([App], [navigation]). Home means Now. Resources keeps Agents and Missions one tap away. Push details in the originating stack with native back and swipe-back; keep Conversation and Terminal full screen and New mission a sheet. The [live web][LiveAgentWorkspace] opens a roster and thread workspace; [web Sidebar]'s broader Inbox is separate. Pick dedicated Now for one place to act.

For [Now], use an inset grouped list: connection and freshness notice, nonzero alert count, then **Somebody is stopped on you**, **Today**, **When there is time**, **Recently closed** in [homeView] order. Rows show wrapping title, labeled kind, requester and waiting context, age and detail disclosure. Replace padded columns and the permanent glyph legend with labels. Retain New and pull to refresh. Show empty only after sync; distinguish loading, failure, offline snapshots and truncation.

The badge and heading count only open alerts for the paired person. Updates ask nothing; messages stay in conversations. Preserve closed items until cleared and explicit Mark read for updates. Pick the phone's rule over web Sidebar's local ReadInbox-on-open to preserve deliberate acknowledgment: opening never reads an update or resolves a request ([Now], [homeView], [web Sidebar]).

Keep alerts answerable above their conversation ([Alerts], [Conversation], [#1959](https://github.com/compoundingtech/smalltalk/issues/1959)), with disclosure and existing capability-gated actions. Preserve full pending-call context before Allow. Coordinate [#2783](https://github.com/compoundingtech/smalltalk/issues/2783) on IDs, count and destination: future push opens detail after sync, showing closed if resolved; notification contents never authorize answers. APNs and Release alert links are separate work.

## Screen slice order

After merge: **Home and alert detail → Agents → Conversation and alert strip → Missions → Fleet → Terminal → Resources and Spaces polish**. Each gets a small PR with before/after screenshots attached using invented data. Home establishes reusable text, color and list primitives. Verify light/dark, large text, VoiceOver, back navigation, live/offline/error states and unchanged actions. Coordinate Home and alerts with #2783 and #1959; keep updates reactive and fix local defects at their cause.

[st3-views]: ../../clients/typescript/st3-views/README.md
[packages/fractal-ui]: ../../packages/fractal-ui/README.md
[LiveAgentWorkspace]: ../../apps/fractal-web/src/web/LiveAgentWorkspace.tsx
[ConversationPane]: ../../apps/fractal-web/src/web/ConversationPane.tsx
[sync-line]: ../../packages/fractal-ui/src/assistant-ui/st3-views/sync-line.ts
[App]: ../../apps/ios/App.tsx
[navigation]: ../../apps/ios/navigation.ts
[ui]: ../../apps/ios/ui.tsx
[theme]: ../../apps/ios/theme.ts
[Now]: ../../apps/ios/screens/Now.tsx
[homeView]: ../../clients/typescript/st3-views/homeView.ts
[web Sidebar]: ../../apps/fractal-web/src/shell/Sidebar.tsx
[Alerts]: ../../apps/ios/screens/Alerts.tsx
[Conversation]: ../../apps/ios/screens/Conversation.tsx
