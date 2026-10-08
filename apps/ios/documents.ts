// st documents, as named in a request's subjects and in messages: `doc/NAME@HASH`.

/** Whether a reference names a document the app can read. */
export function isDocumentName(reference: string | undefined | null): reference is string {
  return !!reference && /^doc\/[^@\s]+@[0-9a-f]+$/.test(reference);
}

/** A document's tab or screen title: its last name part, without the hash. */
export function documentTitle(name: string): string {
  const bare = name.split('@')[0] ?? name;
  return bare.split('/').pop() || bare;
}

/** UTF-8 text from the bytes st returns for a document. */
export function documentText(bytes: ArrayLike<number>): string {
  return new TextDecoder('utf-8').decode(Uint8Array.from(bytes as ArrayLike<number>));
}
