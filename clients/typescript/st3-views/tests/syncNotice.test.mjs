import assert from 'node:assert/strict';
import { catchUpEstimate, envelopeCount, peerSummary, syncLine, syncView } from '@smalltalk/st3-views/syncNotice';

const peer = (host, missing, extra = {}) => ({ host_id: `host/${host}`, peer_only_envelopes: missing, local_only_envelopes: 0, estimated_catch_up_seconds: 840, ...extra });

// The words are the ones stui and `st now` use for the same notice.
assert.equal(envelopeCount(1), '1 envelope');
assert.equal(envelopeCount(124384), '124,384 envelopes');
assert.equal(catchUpEstimate(null), 'estimating time to catch up');
assert.equal(catchUpEstimate(0), 'caught up');
assert.equal(catchUpEstimate(20), 'caught up in under a minute');
assert.equal(catchUpEstimate(61), 'caught up in about 2m');
assert.equal(catchUpEstimate(840), 'caught up in about 14m');
assert.equal(catchUpEstimate(3 * 3600 + 5 * 60), 'caught up in about 3h 5m');
assert.equal(catchUpEstimate(2 * 86400 + 4 * 3600), 'caught up in about 2d 4h');
assert.equal(peerSummary(peer('harbor', 26820)), 'harbor has 26,820 envelopes this host lacks · caught up in about 14m');
assert.equal(peerSummary(peer('quay', 3, { diverged_since: '2026-09-29T10:00:00Z' })), 'quay projects a different graph from the same envelopes');

// A peer with nothing the host lacks is not behind; a diverged peer is diverged, not behind.
const view = syncView({ state: 'catching-up', peers: [peer('harbor', 26820), peer('dock', 0), peer('quay', 5, { diverged_since: '2026-09-29T10:00:00Z' })] });
assert.deepEqual(view.behind, ['harbor has 26,820 envelopes this host lacks · caught up in about 14m']);
assert.deepEqual(view.diverged, ['quay projects a different graph from the same envelopes']);

// No notice, or one with nobody ahead, says nothing; behind and diverged each get a line.
assert.deepEqual(syncView(undefined), { behind: [], diverged: [] });
assert.deepEqual(syncView(null), { behind: [], diverged: [] });
assert.equal(syncLine(syncView({ state: 'catching-up', peers: [peer('dock', 0)] })), undefined);
assert.equal(syncLine({ behind: ['a'], diverged: [] }), 'Catching up: a. What shows here can be out of date until then.');
assert.equal(syncLine({ behind: ['a'], diverged: ['b'] }), "This machine's view may be wrong: b.");
