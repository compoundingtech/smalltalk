import assert from 'node:assert/strict';
import { conversationEntries, deliveredImages, requestImages } from '@smalltalk/st3-views';

// Images in a conversation, as stui shows them (docs/clients/ui-contract.md, "Images").
const sha = 'ab'.repeat(32);
const names = new Map([['person/ada', 'you'], ['agent/example/gardener', 'Gardener']]);
const at = '2026-10-13T10:00:00Z';

// A delivery's <attachment/> is the message's image, read by the message; its path never shows.
{
  const block = `the pond again\n<attachment path="/work/garden/state/attachments/${sha}.png" media_type="image/png" bytes="1269" name="pond.png"/>`;
  const { text, images } = deliveredImages(block, 'message/pond');
  assert.equal(text, 'the pond again');
  assert.deepEqual(images, [{ sha256: sha, message: 'message/pond', mediaType: 'image/png', name: 'pond.png', size: 1269 }]);
  const expired = deliveredImages(`<attachment path="/x/${sha}.png" media_type="image/png" name="old.png" unavailable="expired"/>`, undefined);
  assert.equal(expired.text, '[old.png · expired]');
  assert.deepEqual(expired.images, []);
  const document = deliveredImages('<attachment path="/x/notes.pdf" media_type="application/pdf" name="notes.pdf"/>', 'message/x');
  assert.equal(document.text, '[notes.pdf · application/pdf]');
  assert.equal(deliveredImages('<attachment path="/x/short.png"/>', 'message/x').text, '[attachment · not available here]');
}

// The same through the transcript: a Claude seat's turn carrying st's envelope.
{
  const raw = `<smalltalk-message id="1" from="person/ada" to="agent/example/gardener" subject="pond" graph="message/pond">\nthe pond again\n<attachment path="/work/garden/state/attachments/${sha}.png" media_type="image/png" bytes="1269" name="pond.png"/>\n</smalltalk-message>`;
  const entries = conversationEntries([{ id: 'turn', sequence: 1, revision: 1, timestamp: at, role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text: raw, blocks: [{ id: 'turn/0', kind: 'text', source_type: 'content', payload: { body_ref: true } }] } }], names);
  const mail = entries.find(entry => entry.body.kind === 'mail');
  assert.ok(mail, JSON.stringify(entries));
  assert.equal(mail.body.text, 'the pond again');
  assert.equal(mail.body.images?.[0]?.sha256, sha);
  assert.ok(!JSON.stringify(entries).includes('/work/garden'), 'no path');
}

// An image alone in a turn is its image view, not the words `[image · load from owner]`.
{
  const reference = { ref: 'lawn-ref', media_type: 'image/png', size: 71968, reason: 'on-demand' };
  const entries = conversationEntries([
    { id: 'words', sequence: 1, revision: 1, timestamp: at, role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text: 'what is growing here?', blocks: [{ id: 'words/0', kind: 'text', source_type: 'content', payload: { body_ref: true } }] } },
    { id: 'picture', sequence: 2, revision: 1, timestamp: at, role: 'user', type: 'content', final: true, body: { media_type: 'text/plain', text: '[image · load from owner]', blocks: [{ id: 'picture/0', kind: 'image', source_type: 'image', payload: {}, continuation: reference }] } },
  ], names);
  const picture = entries.find(entry => entry.id === 'picture');
  assert.ok(picture);
  assert.equal(picture.body.text, '');
  assert.equal(picture.content?.[0]?.media_type, 'image/png');
  assert.equal(picture.content?.[0]?.size, 71968);
  assert.ok(!entries.some(entry => 'text' in entry.body && entry.body.text.includes('load from owner')));
}

// An ask's images are read by its person step.
{
  const images = requestImages({ attachments: [
    { sha256: sha, media_type: 'image/png', name: { _tag: 'Some', value: 'north-bed.png' }, size: 2048 },
    { sha256: 'cd'.repeat(32), media_type: 'image/webp', name: { _tag: 'None' }, size: 10 },
  ] }, 'step-run/ask-1/ask');
  assert.deepEqual(images, [
    { sha256: sha, message: 'step-run/ask-1/ask', mediaType: 'image/png', name: 'north-bed.png', size: 2048 },
    { sha256: 'cd'.repeat(32), message: 'step-run/ask-1/ask', mediaType: 'image/webp', size: 10 },
  ]);
  assert.deepEqual(requestImages(undefined, 'step-run/x'), []);
}
