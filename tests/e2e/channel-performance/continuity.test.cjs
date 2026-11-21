const { test } = require('node:test');
const assert = require('node:assert/strict');
const { checkContinuity } = require('./continuity.cjs');

test('detects ten seconds of decoded content replay even when player clock advances', () => {
  const a = { identity: 'broadcast-1', sourceSeconds: 100, playerSeconds: 200 };
  const b = { identity: 'broadcast-1', sourceSeconds: 90, playerSeconds: 201 };
  assert.match(checkContinuity(a, b, 0.5), /content replay/);
});
test('permits a new persisted broadcast identity and explicitly intentional seek', () => {
  const a = { identity: 'a', sourceSeconds: 100, playerSeconds: 200 };
  assert.equal(checkContinuity(a, { identity: 'b', sourceSeconds: 0, playerSeconds: 201 }, 0.5), null);
  assert.equal(checkContinuity(a, { identity: 'a', sourceSeconds: 90, playerSeconds: 201, intentionalSeek: true }, 0.5), null);
});
test('rolling decoded sequence catches injected replay with continuously advancing clock', () => {
  const markers = [100, 101, 102, 103, 94, 95];
  const samples = markers.map((sourceSeconds, i) => ({ identity: 'persisted-row', sourceSeconds, playerSeconds: 200 + i }));
  const failures = samples.slice(1).map((sample, i) => checkContinuity(samples[i], sample, 1)).filter(Boolean);
  assert.equal(failures.length, 1);
  assert.match(failures[0], /103 -> 94/);
  assert.ok(samples.every((sample, i) => !i || sample.playerSeconds > samples[i - 1].playerSeconds));
});
