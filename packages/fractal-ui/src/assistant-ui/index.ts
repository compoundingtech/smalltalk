/** Shared assistant-ui presentation and external-store boundary. Applications own transport and state. */
export * from './EmbraceRuntime.tsx'
export * from './EmbraceThread.tsx'
export * from './EmbraceScrollViewport.tsx'
export * from './composition/Transcript.tsx'
export * from './composition/TranscriptFeedback.tsx'
export * from './EmbraceComposer.tsx'
export * from './EmbraceToolCall.tsx'
export * from './EmbraceToolPreview.tsx'
export * from './EmbraceVirtualConversation.tsx'
export * from './embrace-converter.ts'
export type { SendState, TextItem } from './embrace-data/model.ts'
export type { Draft, DraftToken, MentionToken, CommandToken, SerializedDraft, SlashCommand, SlashCommandId } from './embrace-composer/draft.ts'

/** Reusable round-2 live composition primitives; transport stays application-owned. */
export { AgentRow, StatusGlyph } from './composition/Sidebar.tsx'
export { ThreadHeader, ResizableSplit } from './composition/Shell.tsx'
export { DiffPanel } from './composition/DiffPanel.tsx'
export { Markdown, ResourceChip, HighlightedSource, completeStreamingTail } from './composition/Markdown.tsx'
export type { MarkdownProps, MarkdownImageResolution, MarkdownImageResolver, InlineResource, InlineReferenceRenderer } from './composition/Markdown.tsx'
export { ThinkingEntry } from './composition/ThinkingEntry.tsx'
export { ErrorOverlay, ErrorOverlayHost, useErrorOverlaySurface } from './composition/ErrorOverlay.tsx'
export type { ErrorOverlayNotice } from './composition/ErrorOverlay.tsx'
export { colorVars, typeVars, radiusVars, spaceVars, geometryVars, geometryNumbers } from './composition-tokens.stylex.ts'
export { lightTheme as compositionLightTheme } from './composition-theme.ts'
export { darkTheme as assistantDarkTheme } from './embrace-theme.ts'
export { toolDiffs } from './embrace-tool-preview.ts'
export type { AgentRowData, AgentStatus } from './composition-model.ts'

export { liveComposerDarkTheme, liveAccentTheme } from './embrace-theme.ts'

export { Icon as CompositionIcon } from './composition/Icons'
export { Button as CompositionButton, IconButton as CompositionIconButton } from './composition/Controls'

/** Controlled sidebar row; the host owns clock and data. */
export { SidebarAgentRow, AgentHoverCard, sidebarRowDescription } from './sidebar/SidebarAgentRow.tsx'
export { SidebarStatus } from './sidebar/SidebarStatus.tsx'
export type { SidebarAgentRow as SidebarAgentRowData, SidebarUsage, SidebarDuration, SidebarLastTurn, AgentStatus as SidebarAgentStatus } from './sidebar/model.ts'

export { ResourceCardV1, ResourceChipV1 } from './resources/ResourceCardV1.tsx'
export type { ResourceData, ResourceFile, ResourceMetadata } from './resources/resource-model.ts'

/** Required controlled WF-1; fixture/demo actions are excluded. */
export { WorkLogV1 } from './taste/WorkLogV1.tsx'
export type { WorkLogCallDetailRenderer } from './taste/WorkLogV1.tsx'
export * from './taste/work-log.ts'
