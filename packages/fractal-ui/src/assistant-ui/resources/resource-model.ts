/** Portable resource facts; missing counts and content remain explicit unknowns. */
export type ResourceMetadata<A> = { readonly _tag: 'Known'; readonly value: A } | { readonly _tag: 'Unknown' }
export interface ResourceFile { readonly path: string; readonly added: ResourceMetadata<number>; readonly removed: ResourceMetadata<number>; readonly lines: ResourceMetadata<readonly string[]> }
export interface ResourceData { readonly kind: 'file' | 'diff' | 'pr' | 'worktree'; readonly label: string; readonly detail: string; readonly fileCount: ResourceMetadata<number>; readonly files: readonly ResourceFile[] }
export type ResourceState = 'idle' | 'running' | 'waiting' | 'error' | 'needs-you' | 'interrupted'
