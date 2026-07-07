// errors.ts — typed error subclasses for the embeddable API.
//
// Every StError carries a stable `code` string so JS callers can branch
// on it without the type system, plus an optional `details` payload for
// structured introspection. The CLI layer in src/cli.ts catches these and
// maps them to user-visible messages + exit codes; embedders pattern-match
// on `instanceof` or `code`.

/** Base class for every error raised by the st API or commands. */
export class StError extends Error {
  readonly code: string;
  readonly details?: unknown;

  constructor(code: string, message: string, details?: unknown) {
    super(message);
    // Keep prototype chain working under transpilation that doesn't preserve
    // ES6 class semantics. Harmless under native ESM/Node.
    Object.setPrototypeOf(this, new.target.prototype);
    this.name = new.target.name;
    this.code = code;
    if (details !== undefined) this.details = details;
  }
}

// ─── Agent / argument validation ───────────────────────────────────────
//
// brief-009 item 3 (rename): error class names changed from `Identity*`
// to `Agent*`. The error CODE strings (IDENTITY_REQUIRED etc.) stay
// stable — they're wire format that downstream pattern-matchers
// (instanceof + .code branches) depend on. Old class names remain as
// deprecated aliases pointing at the new classes for one release.

export class AgentRequiredError extends StError {
  constructor() {
    super(
      'IDENTITY_REQUIRED',
      'agent required — set $ST_AGENT (or the legacy $ST_IDENTITY) or pass --from <agent>'
    );
  }
}

export class AgentNotHostedError extends StError {
  readonly identity: string;
  constructor(identity: string) {
    super(
      'IDENTITY_NOT_HOSTED',
      `agent folder missing for ${identity} — create it: mkdir -p $ST_ROOT/${identity}/{inbox,archive}`,
      { identity }
    );
    this.identity = identity;
  }
}

export class InvalidAgentError extends StError {
  readonly value: string;
  constructor(value: string) {
    super('INVALID_IDENTITY', `invalid agent name: ${value}`, { value });
    this.value = value;
  }
}

/** @deprecated Use {@link AgentRequiredError}. */
export const IdentityRequiredError = AgentRequiredError;
/** @deprecated Use {@link AgentNotHostedError}. */
export const IdentityNotHostedError = AgentNotHostedError;
/** @deprecated Use {@link InvalidAgentError}. */
export const InvalidIdentityError = InvalidAgentError;

export class InvalidFilenameError extends StError {
  readonly value: string;
  constructor(value: string) {
    super('INVALID_FILENAME', `invalid filename: ${value}`, { value });
    this.value = value;
  }
}

// ─── Lookup / state errors ─────────────────────────────────────────────

export class MessageNotFoundError extends StError {
  readonly identity: string;
  readonly filename: string;
  constructor(identity: string, filename: string) {
    super(
      'MESSAGE_NOT_FOUND',
      `not found in inbox or archive: ${filename}`,
      { identity, filename }
    );
    this.identity = identity;
    this.filename = filename;
  }
}

export class InvalidStateError extends StError {
  readonly value: string;
  constructor(value: string) {
    super(
      'INVALID_STATE',
      // `unknown` is omitted on purpose: it's a derived state surfaced
      // by mtime staleness and is never settable by the user. `away`
      // (brief-029) joins the settable set.
      'state must be one of: offline, available, busy, away, dnd',
      { value }
    );
    this.value = value;
  }
}

export class InvalidPriorityError extends StError {
  readonly value: string;
  constructor(value: string) {
    super(
      'INVALID_PRIORITY',
      'priority must be one of: low, normal, high',
      { value }
    );
    this.value = value;
  }
}

export class InvalidDurationError extends StError {
  readonly value: string;
  constructor(value: string) {
    super(
      'INVALID_DURATION',
      `invalid duration: ${value} (use e.g. 90d, 12h, 2w)`,
      { value }
    );
    this.value = value;
  }
}

// ─── Sync / peers ──────────────────────────────────────────────────────

export type SyncStage = 'push' | 'pull';

export class SyncFailedError extends StError {
  readonly stage: SyncStage;
  readonly exitCode: number;
  readonly stderr: string | undefined;
  constructor(stage: SyncStage, exitCode: number, stderr: string | undefined, message: string) {
    super('SYNC_FAILED', message, { stage, exitCode, stderr });
    this.stage = stage;
    this.exitCode = exitCode;
    this.stderr = stderr;
  }
}

export class PeersConfigMissingError extends StError {
  readonly path: string;
  constructor(path: string) {
    super(
      'PEERS_CONFIG_MISSING',
      `no peers configured at ${path}`,
      { path }
    );
    this.path = path;
  }
}

export class PeersConfigInvalidError extends StError {
  readonly path: string;
  readonly reason: string;
  constructor(path: string, reason: string) {
    super(
      'PEERS_CONFIG_INVALID',
      `${reason}: ${path}`,
      { path, reason }
    );
    this.path = path;
    this.reason = reason;
  }
}

// ─── Send / archive ────────────────────────────────────────────────────

export class EmptyBodyError extends StError {
  constructor() {
    super('EMPTY_BODY', 'message body is empty (read from stdin)');
  }
}

export class ArchiveConflictError extends StError {
  readonly identity: string;
  readonly filename: string;
  constructor(identity: string, filename: string) {
    super(
      'ARCHIVE_CONFLICT',
      `refuse to archive: archive/${filename} exists and differs from inbox/${filename}. This indicates a violated invariant; resolve by hand.`,
      { identity, filename }
    );
    this.identity = identity;
    this.filename = filename;
  }
}

// ─── Resources (brief-009 item 5) ──────────────────────────────────────

export class ResourceNotFoundError extends StError {
  readonly identity: string;
  readonly filename: string;
  constructor(identity: string, filename: string) {
    super(
      'RESOURCE_NOT_FOUND',
      `resource not found: ${identity}/resources/${filename}`,
      { identity, filename }
    );
    this.identity = identity;
    this.filename = filename;
  }
}

export class InvalidResourceUrlError extends StError {
  readonly value: string;
  constructor(value: string) {
    super(
      'INVALID_RESOURCE_URL',
      `invalid resource url: ${JSON.stringify(value)} (must contain a scheme, e.g. https://, pty://)`,
      { value }
    );
    this.value = value;
  }
}

