// Images a person attaches to a message on the phone: picked from the photo library or pasted.
// st takes PNG, JPEG, GIF and WebP up to 10 MiB each, four to a message
// (docs/st3/attachments.md); anything else is refused here, in words, before it is uploaded.

export type ImageType = 'image/png' | 'image/jpeg' | 'image/gif' | 'image/webp';
export type Picked = { key: string; base64: string; mediaType: ImageType; name?: string; bytes: number };

export const MAX_IMAGES = 4;
export const MAX_BYTES = 10 * 1024 * 1024;
const TYPES: ImageType[] = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];

/** The size of base64 data once decoded. */
export function base64Bytes(base64: string): number {
  const padding = base64.endsWith('==') ? 2 : base64.endsWith('=') ? 1 : 0;
  return Math.floor(base64.length * 3 / 4) - padding;
}

export function decodeBase64(base64: string): Uint8Array {
  const binary = atob(base64);
  return Uint8Array.from(binary, character => character.charCodeAt(0));
}

/** The type the data itself says, by its first bytes, as st checks it on upload. */
export function sniff(base64: string): ImageType | null {
  if (base64.startsWith('iVBORw0KGgo')) return 'image/png';
  if (base64.startsWith('/9j/')) return 'image/jpeg';
  if (base64.startsWith('R0lGOD')) return 'image/gif';
  if (base64.startsWith('UklGR') && atob(base64.slice(8, 16)).slice(2, 6) === 'WEBP') return 'image/webp';
  return null;
}

export function encodeBase64(bytes: Uint8Array): string {
  let binary = '';
  for (let at = 0; at < bytes.length; at += 0x8000) binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
  return btoa(binary);
}

/** An image from base64 data and its type, or why st would not take it. The data's own type wins
 * over the one given: the photo picker hands back JPEG data for what may have been a PNG. */
export function picked(base64: string, mediaType: string, name?: string): Picked | string {
  const type = sniff(base64) ?? (mediaType.toLowerCase() === 'image/jpg' ? 'image/jpeg' : mediaType.toLowerCase());
  if (!TYPES.includes(type as ImageType)) return `${name ?? 'That image'} is ${mediaType || 'of an unknown type'}; st takes PNG, JPEG, GIF or WebP.`;
  const bytes = base64Bytes(base64);
  if (bytes > MAX_BYTES) return `${name ?? 'That image'} is ${megabytes(bytes)}; st takes images up to 10 MB.`;
  return { key: `${type}:${bytes}:${base64.slice(0, 32)}:${base64.slice(-32)}`, base64, mediaType: type as ImageType, ...(name ? { name } : {}), bytes };
}

/** An image from a `data:` URI, as the clipboard gives it. */
export function fromDataUri(uri: string, name?: string): Picked | string {
  const match = /^data:([^;,]+);base64,(.*)$/s.exec(uri);
  if (!match) return 'The clipboard did not hold an image st can read.';
  return picked(match[2], match[1], name);
}

/** The images to send after adding more: the same image once, at most four, and why any were left out. */
export function addImages(current: Picked[], more: Array<Picked | string>): { images: Picked[]; refused: string } {
  const images = [...current];
  const reasons: string[] = [];
  for (const item of more) {
    if (typeof item === 'string') { reasons.push(item); continue; }
    if (images.some(image => image.key === item.key)) continue;
    if (images.length >= MAX_IMAGES) { reasons.push(`A message carries at most ${MAX_IMAGES} images.`); break; }
    images.push(item);
  }
  return { images, refused: [...new Set(reasons)].join(' ') };
}

export function megabytes(bytes: number): string {
  return bytes >= 1024 * 1024 ? `${(bytes / (1024 * 1024)).toFixed(1)} MB` : `${Math.max(1, Math.round(bytes / 1024))} KB`;
}
