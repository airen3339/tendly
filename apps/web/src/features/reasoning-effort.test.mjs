import assert from 'node:assert/strict';
import { test } from 'node:test';
import { modelEfforts } from './reasoning-effort.ts';

test('Claude exposes its native effort vocabulary without requiring a guessed model catalog', () => {
  assert.deepEqual(modelEfforts('claude_code', 'claude-sonnet', undefined), ['low', 'medium', 'high', 'xhigh', 'max']);
});

test('Codex only exposes levels advertised by the selected model', () => {
  const catalog = { models: [{ id: 'o4-mini', name: 'o4-mini', efforts: ['low', 'high', 'bogus'] }], source_url: 'codex://model/list', has_more: false };
  assert.deepEqual(modelEfforts('codex', 'o4-mini', catalog), ['low', 'high']);
  assert.deepEqual(modelEfforts('codex', 'unknown', catalog), []);
});

test('an existing effort remains visible while a refreshed catalog is temporarily missing it', () => {
  const catalog = { models: [{ id: 'model', name: 'model', efforts: ['low'] }], source_url: 'fixture://models', has_more: false };
  assert.deepEqual(modelEfforts('codex', 'model', catalog, 'high'), ['low', 'high']);
});

test('custom endpoints permit manual effort without model metadata, but honor advertised levels', () => {
  assert.deepEqual(modelEfforts('codex', 'custom', undefined, undefined, true), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(modelEfforts('pi', 'custom', undefined, undefined, true), ['low', 'medium', 'high']);
  const catalog = { models: [{ id: 'custom', name: 'custom', efforts: ['low', 'high'] }], source_url: 'https://example.invalid/models', has_more: false };
  assert.deepEqual(modelEfforts('codex', 'custom', catalog, undefined, true), ['low', 'high']);
  assert.deepEqual(modelEfforts('codex', 'custom', undefined), []);
});
