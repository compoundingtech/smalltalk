import type { DocumentContent, DocumentItem, DocumentsPage } from '@smalltalk/st3-client';

// What a document view shows, read through the client's `document.get` and `documents.list`.
// The same model as crates/st3-ui-model/src/documents.rs; fixtures/clients/documents-v1.json
// pins both. A document is shown as its author wrote it: the text is never filtered or rewritten.

/** A `doc/…` reference found in text, as written. */
export type DocumentLink = { reference: string; name: string; version: string | null };

const HASH = /^[0-9a-fA-F]{64}$/;
const NAME_BYTE = /[A-Za-z0-9/._~+-]/;

/** Splits `doc/NAME@HASH` or `doc/NAME`, or `null` when it is not a document reference. */
export function parseDocumentReference(reference: string): { name: string; version: string | null } | null {
  const at = reference.lastIndexOf('@');
  const name = at < 0 ? reference : reference.slice(0, at);
  const version = at < 0 ? null : reference.slice(at + 1);
  const validName = name.startsWith('doc/') && name.length > 4 && !name.includes('..') && !name.includes('@') && !name.endsWith('/');
  const validVersion = version === null || HASH.test(version);
  return validName && validVersion ? { name, version } : null;
}

/** Every `doc/…` reference in `text`, in order of first appearance, each once. */
export function documentLinks(text: string): DocumentLink[] {
  const links: DocumentLink[] = [];
  let at = 0;
  for (;;) {
    const start = text.indexOf('doc/', at);
    if (start < 0) return links;
    at = start + 4;
    const before = start > 0 ? text[start - 1] : '';
    if (before && (NAME_BYTE.test(before) || before === '@')) continue;
    let end = start + 4;
    while (end < text.length && NAME_BYTE.test(text[end])) end += 1;
    while (end > start + 4 && (text[end - 1] === '.' || text[end - 1] === '/')) end -= 1;
    let referenceEnd = end;
    if (text[end] === '@') {
      const hash = text.slice(end + 1, end + 65);
      const after = text[end + 65] ?? '';
      if (HASH.test(hash) && !/[A-Za-z0-9]/.test(after)) referenceEnd = end + 65;
    }
    at = Math.max(at, referenceEnd);
    const reference = text.slice(start, referenceEnd);
    const parsed = parseDocumentReference(reference);
    if (parsed && !links.some(link => link.reference === reference)) links.push({ reference, ...parsed });
  }
}

/** Why a document cannot be shown. Each says so in words; none is a blank page. */
export type DocumentUnavailable = 'not-found' | 'not-synced' | 'remote-unavailable' | 'not-text';

/** Which state a failed `document.get` means: the HTTP status (null when nothing answered) and the error code. */
export function documentUnavailable(status: number | null, code: string): DocumentUnavailable {
  if (code === 'document-not-synced') return 'not-synced';
  if (status === 404 || code === 'not-found') return 'not-found';
  return 'remote-unavailable';
}

export function explainUnavailable(why: DocumentUnavailable, reference: string): string {
  switch (why) {
    case 'not-found': return `No document named ${reference} is on this host.`;
    case 'not-synced': return `${reference} has not reached this host yet. It shows once it syncs.`;
    case 'remote-unavailable': return `The host did not answer, so ${reference} cannot be read now.`;
    case 'not-text': return `${reference} is not text, so it cannot be shown.`;
  }
}

export type OpenDocument = {
  reference: string;
  name: string;
  version: string;
  shortVersion: string;
  /** When this version was bound to its name; null from a daemon that does not say. */
  changedAtUnixMs: number | null;
  latest: boolean | null;
  owner: string | null;
  size: number;
  /** The Markdown exactly as stored. */
  text: string;
  links: DocumentLink[];
};

export type DocumentView =
  | { state: 'loading'; reference: string }
  | ({ state: 'ready' } & OpenDocument)
  | { state: 'unavailable'; reference: string; why: DocumentUnavailable; explanation: string };

export const loadingDocument = (reference: string): DocumentView => ({ state: 'loading', reference });

export const unavailableDocument = (reference: string, why: DocumentUnavailable): DocumentView =>
  ({ state: 'unavailable', reference, why, explanation: explainUnavailable(why, reference) });

const utf8 = new TextDecoder('utf-8', { fatal: true });

/** The view of a `document.get` answer for `requested`. */
export function documentView(requested: string, content: DocumentContent): DocumentView {
  let text: string;
  try {
    text = utf8.decode(Uint8Array.from(content.bytes));
  } catch {
    return unavailableDocument(requested, 'not-text');
  }
  const at = content.reference.lastIndexOf('@');
  const version = content.version ?? (at < 0 ? '' : content.reference.slice(at + 1));
  return {
    state: 'ready',
    reference: content.reference,
    name: content.name ?? (at < 0 ? content.reference : content.reference.slice(0, at)),
    version,
    shortVersion: version.slice(0, 12),
    changedAtUnixMs: content.changed_at_unix_ms ?? null,
    latest: content.latest ?? null,
    owner: content.owner ?? null,
    size: content.size ?? content.bytes.length,
    text,
    links: documentLinks(text),
  };
}

export type DocumentRow = {
  reference: string; name: string; version: string; shortVersion: string;
  changedAtUnixMs: number; size: number; latest: boolean; owner: string | null;
};

export const documentRow = (item: DocumentItem): DocumentRow => ({
  reference: item.reference, name: item.name, version: item.version, shortVersion: item.version.slice(0, 12),
  changedAtUnixMs: item.changed_at_unix_ms, size: item.size, latest: item.latest, owner: item.owner ?? null,
});

/** The documents list under one prefix, a page at a time; loading, empty and failed stay apart. */
export type DocumentList = {
  prefix: string | null;
  rows: DocumentRow[];
  loaded: boolean;
  nextCursor: string | null;
  failed: DocumentUnavailable | null;
};

export const emptyDocumentList = (prefix: string | null = null): DocumentList =>
  ({ prefix, rows: [], loaded: false, nextCursor: null, failed: null });

/** Adds a page; a row already listed keeps its place. */
export function pushDocumentPage(list: DocumentList, page: DocumentsPage): DocumentList {
  const rows = [...list.rows];
  for (const item of page.items) if (!rows.some(row => row.reference === item.reference)) rows.push(documentRow(item));
  return { ...list, rows, loaded: true, failed: null, nextCursor: page.has_more ? page.next_cursor ?? null : null };
}

export const failDocumentList = (list: DocumentList, why: DocumentUnavailable): DocumentList => ({ ...list, failed: why });

/** The words for a list with no rows to show, or null when it has rows. */
export function documentListPlaceholder(list: DocumentList): string | null {
  if (list.rows.length > 0) return null;
  const under = list.prefix ?? 'doc/';
  if (list.failed === 'remote-unavailable') return 'The host did not answer, so the documents cannot be listed now.';
  if (list.failed) return explainUnavailable(list.failed, under);
  return list.loaded ? `No documents under ${under}.` : 'Loading…';
}

/** Opening a document already on the trail goes back to it, so a link cycle never grows the trail. */
export function openOnTrail(trail: readonly string[], reference: string): string[] {
  const index = trail.indexOf(reference);
  return index < 0 ? [...trail, reference] : trail.slice(0, index + 1);
}

/** Goes back one document; the first stays. */
export const backOnTrail = (trail: readonly string[]): string[] => (trail.length > 1 ? trail.slice(0, -1) : [...trail]);
