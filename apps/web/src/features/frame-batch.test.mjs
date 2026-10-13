import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createFrameBatch } from './frame-batch.ts';

test('pieces that arrive within a frame are applied together, once', () => {
  const applied = [], frames = [];
  const batch = createFrameBatch(items => applied.push(items), run => frames.push(run));
  for (const piece of ['a', 'b', 'c']) batch.push(piece);
  assert.equal(frames.length, 1, 'one frame asked for');
  assert.deepEqual(applied, []);
  frames.shift()();
  assert.deepEqual(applied, [['a', 'b', 'c']]);
  batch.push('d');
  assert.equal(frames.length, 1);
  frames.shift()();
  assert.deepEqual(applied, [['a', 'b', 'c'], ['d']]);
});

test('a flush applies what is waiting now, and the frame that follows finds nothing', () => {
  const applied = [], frames = [];
  const batch = createFrameBatch(items => applied.push(items), run => frames.push(run));
  batch.push('a'); batch.flush();
  assert.deepEqual(applied, [['a']]);
  frames.shift()();
  assert.deepEqual(applied, [['a']]);
  batch.push('b'); batch.clear(); frames.shift()?.();
  assert.equal(batch.size, 0);
  assert.deepEqual(applied, [['a']]);
});
