import { test } from 'node:test';
import assert from 'node:assert/strict';
import { occurrences, parseSearchResult, rowForHit, stepHit } from './conversation-search.ts';

test('a search answer is read strictly', () => {
  assert.deepEqual(parseSearchResult({ hits: [{ seq: 3, kind: 'tool', id: 't', snippet: 'x' }], more: false }), { hits: [{ seq: 3, kind: 'tool', id: 't', snippet: 'x' }], more: false });
  for (const bad of [null, { hits: [] }, { hits: [{ seq: 'x', kind: 'tool', id: 't', snippet: '' }], more: false }, { hits: [{ seq: 1, kind: 'approval', id: 'a', snippet: '' }], more: false }]) assert.throws(() => parseSearchResult(bad), /Invalid search/);
});

test('a hit is found in its row, and a call folded inside others names what to open', () => {
  const rows = [
    { type: 'message', id: 'm', role: 'user', text: 'hi' },
    { type: 'tool_run', id: 'a', tools: [{ type: 'tool', id: 'a', name: 'Agent', status: 'completed', text: '', children: [{ type: 'tool', id: 'b', name: 'Bash', status: 'completed', text: '', children: [{ type: 'tool', id: 'c', name: 'Read', status: 'completed', text: '' }] }] }] },
  ];
  assert.deepEqual(rowForHit(rows, { seq: 1, kind: 'message', id: 'm', snippet: '' }), { index: 0, path: [] });
  assert.deepEqual(rowForHit(rows, { seq: 2, kind: 'tool', id: 'c', snippet: '' }), { index: 1, path: ['a', 'b', 'c'] });
  assert.equal(rowForHit(rows, { seq: 3, kind: 'tool', id: 'gone', snippet: '' }), undefined);
});

test('stepping through hits goes around in both directions', () => {
  assert.equal(stepHit(0, -1, 1), -1);
  assert.equal(stepHit(3, -1, 1), 0);
  assert.equal(stepHit(3, -1, -1), 2);
  assert.equal(stepHit(3, 2, 1), 0);
  assert.equal(stepHit(3, 0, -1), 2);
});

test('occurrences are found without regard to case, each once', () => {
  assert.deepEqual(occurrences('Retry, then retry again', 'retry'), [[0, 5], [12, 17]]);
  assert.deepEqual(occurrences('aaaa', 'aa'), [[0, 2], [2, 4]]);
  assert.deepEqual(occurrences('重试间隔', '试间'), [[1, 3]]);
  assert.deepEqual(occurrences('anything', '  '), []);
});
