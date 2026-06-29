// @myobie/coord — programmatic API.
//
// Embedders import from this entry point:
//   import { createCoord, asIdentity } from '@myobie/coord';
//
// The CLI entry point (`bin/coord` → `src/cli.ts`) is invoked separately;
// it is not re-exported here.

export const VERSION = '0.8.0';

// ─── Factory + handle ──────────────────────────────────────────────────

export {
  createCoord,
  type ArchiveOptions,
  type Coord,
  type CoordOptions,
  type FanOutBidiItem,
  type FanOutItem,
  type LsOptions,
  type OrphanItem,
  type ReadOptions,
  type SendOptions,
  type SyncResult,
  type ThreadOptions,
  type TrimOptions,
  type WatchOptions,
} from './lib.ts';

// ─── Public types + branded primitives ─────────────────────────────────

// brief-009 item 3 (rename): asAgent / isAgent / Agent are the
// preferred names; asIdentity / isIdentity / Identity remain as
// @deprecated aliases pointing at the same brand for one release
// cycle. Both spellings work; the deprecation is JSDoc-only.
export {
  asAgent,
  asFilename,
  asIdentity,
  deriveTo,
  deriveTs,
  isAgent,
  isFilename,
  isIdentity,
  isState,
  parsePeer,
  PRIORITIES,
  STATES,
  type Agent,
  type Filename,
  type Identity,
  type Message,
  type MessageWithLocation,
  type ParsedPeer,
  type Peer,
  type PeerKind,
  type Priority,
  type Resource,
  type ResourceWithLocation,
  type State,
  type WatchEvent,
} from './types.ts';

// ─── Errors ────────────────────────────────────────────────────────────

// Identity*Error classes are @deprecated aliases of the new Agent*Error
// classes — same constructor, same instances; safe to swap.
export {
  AgentNotHostedError,
  AgentRequiredError,
  ArchiveConflictError,
  CoordError,
  EmptyBodyError,
  IdentityNotHostedError,
  IdentityRequiredError,
  InvalidAgentError,
  InvalidDurationError,
  InvalidFilenameError,
  InvalidIdentityError,
  InvalidPriorityError,
  InvalidResourceUrlError,
  InvalidStateError,
  MessageNotFoundError,
  PeersConfigInvalidError,
  PeersConfigMissingError,
  ResourceNotFoundError,
  type SyncStage,
  SyncFailedError,
} from './errors.ts';

// ─── common.ts conveniences ────────────────────────────────────────────

export {
  emitFrontmatter,
  parseFrontmatter,
  RESERVED_NAMES,
  validFilename,
  validIdentity,
  yamlQuote,
} from './common.ts';

// ─── Agents + overview shapes (brief-028, renamed in brief-009 item 3) ─

export {
  type AgentSummary,
  type AgentSummaryEnriched,
  type MemberSummary,
  type MemberSummaryEnriched,
} from './commands/agents.ts';

export {
  type ActivityKind,
  type Overview,
  type OverviewActivity,
  type OverviewInbox,
  type OverviewInboxOldest,
} from './commands/overview.ts';
