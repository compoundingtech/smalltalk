/** Application-facing shapes only; no workshop fixtures enter the live graph. */
export type AgentStatus = 'working' | 'done' | 'attention' | 'failed' | 'input' | 'idle' | 'unknown'
export interface AgentRowData { readonly id: string; readonly title: string; readonly agent: string; readonly status: AgentStatus; readonly elapsed: string; readonly time: string }
export interface FolderGroupData { readonly id: string; readonly label: string; readonly rows: readonly AgentRowData[] }
export const surfaceOptions = [{ key: 'diff', label: 'Changes', hint: '' }] as const
