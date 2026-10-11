#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { conversationEntries, simplify, headerLine } from '../../clients/typescript/st3-views/index.ts';

if (process.argv.length !== 3) throw new Error('Usage: node render.mjs PAGE.json');
const envelope = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const page = envelope.value;
if (!Array.isArray(page?.items)) throw new Error('Expected a st3.client.v0 timeline-page envelope');
const header = headerLine(page.header, new Date().toISOString());
if (header !== null) console.log(header);
const entries = conversationEntries(page.items, new Map());
// Open runs through simplify itself; otherwise consecutive tools hide their typed rows.
const opened = new Set(simplify(entries, new Set()).filter(row => row.kind === 'bundle').map(row => row.id));
const printBody = body => {
  if (body.kind === 'tool') {
    console.log(body.title);
    for (const line of body.output) console.log(line);
  } else if (body.kind === 'mail') {
    for (const line of body.text.split('\n')) console.log(`${body.from}: ${line}`);
  } else if (typeof body.text === 'string') {
    for (const line of body.text.split('\n')) console.log(line);
  } else {
    throw new Error(`Unhandled conversation body: ${body.kind}`);
  }
};
for (const row of simplify(entries, opened)) {
  if (row.kind === 'bundle') console.log(`tools ${row.calls.length} · ok ${row.ok} · failed ${row.failed} · running ${row.running}`);
  else printBody(row.kind === 'call' ? row.tool : row.entry.body);
}
