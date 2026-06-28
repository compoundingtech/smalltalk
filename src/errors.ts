// errors.ts — typed error subclasses for the embeddable API.
//
// Every CoordError carries a stable `code` string so JS callers can branch
// on it without the type system, plus an optional `details` payload for
// structured introspection. The CLI layer in src/cli.ts catches these and
// maps them to user-visible messages + exit codes; embedders pattern-match
// on `instanceof` or `code`.

/** Base class for every error raised by the coord API or commands. */
export class CoordError extends Error {
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

// ─── Identity / argument validation ────────────────────────────────────

export class IdentityRequiredError extends CoordError {
  constructor() {
    super(
      'IDENTITY_REQUIRED',
      'identity required — set COORD_IDENTITY or pass --from <id>'
    );
  }
}

export class IdentityNotHostedError extends CoordError {
  readonly identity: string;
  constructor(identity: string) {
    super(
      'IDENTITY_NOT_HOSTED',
      `identity folder missing for ${identity} — create it: mkdir -p $COORD_ROOT/${identity}/{inbox,archive}`,
      { identity }
    );
    this.identity = identity;
  }
}

export class InvalidIdentityError extends CoordError {
  readonly value: string;
  constructor(value: string) {
    super('INVALID_IDENTITY', `invalid identity: ${value}`, { value });
    this.value = value;
  }
}

export class InvalidFilenameError extends CoordError {
  readonly value: string;
  constructor(value: string) {
    super('INVALID_FILENAME', `invalid filename: ${value}`, { value });
    this.value = value;
  }
}

// ─── Lookup / state errors ─────────────────────────────────────────────

export class MessageNotFoundError extends CoordError {
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

export class InvalidStateError extends CoordError {
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

export class InvalidPriorityError extends CoordError {
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

export class InvalidDurationError extends CoordError {
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

export class SyncFailedError extends CoordError {
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

export class PeersConfigMissingError extends CoordError {
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

export class PeersConfigInvalidError extends CoordError {
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

export class EmptyBodyError extends CoordError {
  constructor() {
    super('EMPTY_BODY', 'message body is empty (read from stdin)');
  }
}

export class ArchiveConflictError extends CoordError {
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

