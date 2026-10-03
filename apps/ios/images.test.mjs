import assert from 'node:assert/strict';
import { addImages, base64Bytes, decodeBase64, encodeBase64, fromDataUri, MAX_BYTES, megabytes, picked, sniff } from './images.ts';

const png = Buffer.from('a small png stand-in').toString('base64');

// Sizes and bytes as the upload will see them.
assert.equal(base64Bytes(png), 'a small png stand-in'.length);
assert.equal(Buffer.from(decodeBase64(png)).toString(), 'a small png stand-in');
const large = Uint8Array.from({ length: 100_000 }, (_, index) => index % 251);
assert.equal(encodeBase64(large), Buffer.from(large).toString('base64'));

// A pasted image arrives as a data URI; its type and bytes come from it.
const pasted = fromDataUri(`data:image/png;base64,${png}`, 'Pasted image');
assert.equal(typeof pasted, 'object');
assert.equal(pasted.mediaType, 'image/png');
assert.equal(pasted.name, 'Pasted image');
assert.equal(fromDataUri('not an image'), 'The clipboard did not hold an image st can read.');

// st's limits are said before anything is uploaded.
assert.equal(picked(png, 'image/jpg').mediaType, 'image/jpeg');
assert.match(picked(png, 'image/heic', 'IMG_1.HEIC'), /IMG_1.HEIC is image\/heic; st takes PNG, JPEG, GIF or WebP/);
const huge = 'A'.repeat(Math.ceil((MAX_BYTES + 3) * 4 / 3));
assert.match(picked(huge, 'image/png'), /is 10\.0 MB; st takes images up to 10 MB/);

// The same image once; at most four; refusals in words.
const one = picked(png, 'image/png');
const others = ['b', 'c', 'd', 'e'].map(letter => picked(Buffer.from(letter.repeat(40)).toString('base64'), 'image/jpeg'));
let { images, refused } = addImages([one], [one, 'Not that one.']);
assert.equal(images.length, 1);
assert.equal(refused, 'Not that one.');
({ images, refused } = addImages(images, others));
assert.equal(images.length, 4);
assert.equal(refused, 'A message carries at most 4 images.');

// The data's own type wins: the picker hands back JPEG for what was a PNG.
const jpeg = Buffer.from([0xff, 0xd8, 0xff, 0xe0, 1, 2, 3]).toString('base64');
const realPng = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0]).toString('base64');
const webp = Buffer.from('RIFF\x10\x00\x00\x00WEBPVP8 ', 'latin1').toString('base64');
assert.equal(sniff(jpeg), 'image/jpeg');
assert.equal(sniff(realPng), 'image/png');
assert.equal(sniff(webp), 'image/webp');
assert.equal(picked(jpeg, 'image/png', 'IMG_2.PNG').mediaType, 'image/jpeg');

assert.equal(megabytes(1536 * 1024), '1.5 MB');
assert.equal(megabytes(300), '1 KB');
