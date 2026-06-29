// @myobie/coord — programmatic API.
//
// Embedders import from this entry point:
//   import { createCoord, asIdentity } from '@myobie/coord';
//
// The CLI entry point (`bin/coord` → `src/cli.ts`) is invoked separately;
// it is not re-exported here.

export const VERSION = '0.6.0';

// ─── Factory + handle ──────────────────────────────────────────────────

export {
  createCoord,
  type Coord,
  type CoordOptions,
  type FanOutBidiItem,
  type FanOutItem,
  type LsOptions,
  type ReadOptions,
  type SendOptions,
  type SyncResult,
  type ThreadOptions,
  type TrimOptions,
  type WatchOptions,
} from './lib.ts';

// ─── Public types + branded primitives ─────────────────────────────────

export {
  asFilename,
  asIdentity,
  deriveTo,
  deriveTs,
  isFilename,
  isIdentity,
  isState,
  parsePeer,
  PRIORITIES,
  STATES,
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

export {
  ArchiveConflictError,
  CoordError,
  EmptyBodyError,
  IdentityNotHostedError,
  IdentityRequiredError,
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

// ─── Members + overview shapes (brief-028) ─────────────────────────────

export {
  type MemberSummary,
  type MemberSummaryEnriched,
} from './commands/members.ts';

export {
  type ActivityKind,
  type Overview,
  type OverviewActivity,
  type OverviewInbox,
  type OverviewInboxOldest,
} from './commands/overview.ts';
