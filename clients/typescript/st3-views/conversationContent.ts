import type { ConversationContentChunk, ConversationContentRef } from '@smalltalk/st3-client';

/** References can name a payload, metadata/view subtree, or entire body. Never merge by guessing. */
export const contentReferences = (value: unknown): ConversationContentRef[] => {
  const refs = new Map<string, ConversationContentRef>();
  const visit = (value: unknown): void => {
    if (!value || typeof value !== 'object') return;
    if (Array.isArray(value)) { value.forEach(visit); return; }
    const object = value as Record<string, unknown>;
    if (typeof object.ref === 'string' && typeof object.media_type === 'string') {
      refs.set(object.ref, { ref: object.ref, media_type: object.media_type, ...(typeof object.reason === 'string' ? { reason: object.reason } : {}), ...(typeof object.size === 'number' ? { size: object.size } : {}) });
    }
    Object.values(object).forEach(visit);
  };
  visit(value);
  return [...refs.values()];
};

export type LoadedContent = { bytes: Uint8Array; mediaType: string };
/** Assemble from zero: JSON chunks contain the entire original value, not a clipped suffix.
 * Bytes remain local to the caller; this helper never persists or globally caches content. */
export const loadConversationContent = async (
  reference: ConversationContentRef,
  fetchChunk: (reference: string, offset: number) => Promise<ConversationContentChunk>,
): Promise<LoadedContent> => {
  const parts: Uint8Array[] = [];
  let offset = 0;
  let size = -1;
  let mediaType = '';
  for (;;) {
    const chunk = await fetchChunk(reference.ref, offset);
    if (chunk.kind !== 'conversation-content-chunk' || chunk.ref !== reference.ref || chunk.offset !== offset
      || !Number.isSafeInteger(chunk.size) || chunk.size < 0
      || (size !== -1 && size !== chunk.size) || (mediaType !== '' && mediaType !== chunk.media_type)) {
      throw new Error('The server returned inconsistent content chunks.');
    }
    size = chunk.size;
    mediaType = chunk.media_type;
    const decoded = atob(chunk.data);
    const bytes = Uint8Array.from(decoded, character => character.charCodeAt(0));
    const end = offset + bytes.length;
    if (end > size || (chunk.next_offset != null && (chunk.next_offset !== end || end <= offset || end >= size))
      || (chunk.next_offset == null && end !== size)) throw new Error('The server returned incomplete content.');
    parts.push(bytes);
    if (chunk.next_offset == null) break;
    offset = chunk.next_offset;
  }
  const bytes = new Uint8Array(size);
  offset = 0;
  for (const part of parts) { bytes.set(part, offset); offset += part.length; }
  return { bytes, mediaType };
};

/** Only passive raster image formats are rendered inline, including octet-stream responses. */
export const contentImageUri = ({ bytes, mediaType }: LoadedContent): string => {
  const starts = (...signature: number[]): boolean => signature.every((byte, index) => bytes[index] === byte);
  const detected = starts(137, 80, 78, 71, 13, 10, 26, 10) ? 'image/png'
    : starts(255, 216, 255) ? 'image/jpeg'
    : starts(71, 73, 70, 56) && (bytes[4] === 55 || bytes[4] === 57) && bytes[5] === 97 ? 'image/gif'
    : starts(82, 73, 70, 70) && bytes[8] === 87 && bytes[9] === 69 && bytes[10] === 66 && bytes[11] === 80 ? 'image/webp'
    : undefined;
  if (!detected) throw new Error(`This image format cannot be displayed inline (${mediaType}).`);
  let binary = '';
  for (let offset = 0; offset < bytes.length; offset += 8192) binary += String.fromCharCode(...bytes.subarray(offset, offset + 8192));
  return `data:${detected};base64,${btoa(binary)}`;
};

export const contentJsonValue = (content: LoadedContent): unknown => JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(content.bytes));
export const contentJsonText = (content: LoadedContent): string => JSON.stringify(contentJsonValue(content), null, 2);
