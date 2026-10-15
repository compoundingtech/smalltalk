// What the iOS feed builds from each captured case: the view model the app's Conversation screen
// draws (`conversationEntries` from @smalltalk/st3-views), and which image view each entry gets.
//
// Usage (from the repository root):
//   node --experimental-transform-types scripts/conversation-images-audit/ios-views.mjs [FIXTURE_DIR]
//
// FIXTURE_DIR defaults to fixtures/clients/conversation-images (written by `run capture`).
// apps/ios/screens/Conversation.tsx draws a mail entry's `images` with MailImageView and each
// `content` reference whose reason is on-demand or whose type is an image with
// ConversationImageView; this prints that decision for every entry that carries an image.
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { conversationEntries } from '../../clients/typescript/st3-views/index.ts';

const dir = process.argv[2] ?? 'fixtures/clients/conversation-images';
const names = new Map([['person/ada', 'you'], ['agent/example/gardener', 'Gardener']]);
const brief = (text) => JSON.stringify((text ?? '').replace(/\s+/g, ' ').slice(0, 70));

for (const file of readdirSync(dir).filter(name => name.endsWith('.json')).sort()) {
  const page = JSON.parse(readFileSync(join(dir, file), 'utf8'));
  console.log(`== ${file}`);
  for (const entry of conversationEntries(page.items, names)) {
    const body = entry.body;
    const images = body.kind === 'mail' ? body.images ?? [] : [];
    const content = (entry.content ?? []).filter(reference => reference.reason === 'on-demand' || reference.media_type.startsWith('image/'));
    const text = body.text ?? body.title ?? (body.output ?? []).join(' ');
    const marks = /\[image|\[media|<attachment/.test(JSON.stringify(body));
    if (!images.length && !content.length && !marks) continue;
    console.log(`${entry.id}  ${body.kind}  ${brief(text)}`);
    for (const image of images) {
      console.log(`  MailImageView  ${image.name ?? 'image'} · ${image.size} bytes · ${image.mediaType}${'origin' in image ? '' : ' · origin dropped'}`);
    }
    for (const reference of content) {
      console.log(`  ConversationImageView  ${reference.media_type} · size ${reference.size ?? 'unknown'} · ${reference.reason}`);
    }
    if (marks && !images.length && !content.length) console.log('  no image view: the text names an image');
  }
}
