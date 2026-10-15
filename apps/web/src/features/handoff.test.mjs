import test from 'node:test';
import assert from 'node:assert/strict';
import { handoffChoices, handoffDraft, handoffTitle, handoffTranscript, HANDOFF_TRANSCRIPT_BYTES, profileNote } from './handoff.ts';
import { AGENT_CLIENTS } from './clients.ts';

const labels = { user: 'You', assistant: 'Claude Code', omitted: 'earlier messages omitted' };
const bytes = text => new TextEncoder().encode(text).byteLength;

test('a short conversation is carried over whole, in order, with who said what', () => {
  const { text, omitted } = handoffTranscript([
    { role: 'user', text: 'Rename the helper.' },
    { role: 'assistant', text: 'Renamed it in three files.' },
    { role: 'user', text: '   ' },
  ], labels);
  assert.equal(omitted, false);
  assert.equal(text, 'You:\nRename the helper.\n\nClaude Code:\nRenamed it in three files.');
});

// The server refuses a message over 32768 bytes, and Chinese text is three
// bytes a character, so the budget is counted in bytes rather than characters.
test('a long conversation keeps its latest turns and says the start was dropped', () => {
  const messages = Array.from({ length: 400 }, (_, index) => ({ role: index % 2 ? 'assistant' : 'user', text: `第${index}轮：` + '这是一段比较长的中文内容。'.repeat(8) }));
  const { text, omitted } = handoffTranscript(messages, labels);
  assert.equal(omitted, true);
  assert.ok(text.startsWith('(earlier messages omitted)'));
  assert.ok(text.includes('第399轮'));
  assert.ok(!text.includes('第0轮'));
  assert.ok(bytes(text) <= HANDOFF_TRANSCRIPT_BYTES + 64);
  assert.ok(bytes(text) + 2048 < 32768);
});

test('one message larger than the whole budget keeps its end, cut between characters', () => {
  const huge = '头' + '中'.repeat(20000) + '尾';
  const { text, omitted } = handoffTranscript([{ role: 'assistant', text: huge }], labels, 3000);
  assert.equal(omitted, true);
  assert.ok(text.endsWith('尾'));
  assert.ok(!text.includes('头'));
  assert.ok(!text.includes('�'));
  assert.ok(bytes(text) <= 3000 + 64);
});

const t = (text, values = {}) => text.replace(/\{(\w+)\}/g, (_, key) => String(values[key]));

test('a profile says what choosing it means', () => {
  assert.equal(profileNote({ native_config: { config_dir: '~/.codex' } }, t), 'Uses the sign-in and settings in ~/.codex');
  assert.equal(profileNote({ endpoint_url: 'https://gateway.example' }, t), 'Custom endpoint · https://gateway.example');
  assert.equal(profileNote({}, t), 'Custom endpoint profile');
});

test('a conversation can move to every other client, under each profile and a fresh home', () => {
  const profiles = [{ id: 'work', name: 'Work', provider: 'codex', endpoint_url: 'https://gw' }, { id: 'mine', name: 'Mine', provider: 'claude_code' }];
  const choices = handoffChoices('claude_code', profiles, t);
  assert.ok(choices.every(choice => choice.provider !== 'claude_code'), 'never to the client it is already in');
  assert.deepEqual(choices.filter(choice => choice.provider === 'codex').map(choice => [choice.profileId, choice.name]), [['work', 'Work'], [null, 'Native · isolated configuration']]);
  assert.equal(choices.find(choice => choice.profileId === 'work').note, 'Custom endpoint · https://gw');
  assert.deepEqual(new Set(choices.map(choice => choice.provider)), new Set(AGENT_CLIENTS.filter(provider => provider !== 'claude_code')));
});

test('a terminal has no conversation to carry', () => {
  assert.deepEqual(handoffChoices('terminal', [], t), []);
});

test('the new session is named for where it went', () => {
  assert.equal(handoffTitle('Fix login', { provider: 'codex', profileId: null, name: '', note: '' }, t), 'Fix login · Codex');
  assert.equal(handoffTitle('Fix login', { provider: 'codex', profileId: null, name: '', note: '', branch: 'feature/x' }, t), 'Fix login · feature/x');
});

test('the first message carries the transcript and leaves the next step to the user', () => {
  const choice = { provider: 'codex', profileId: null, name: '', note: '' };
  const text = handoffDraft([{ role: 'user', text: 'Rename it.' }, { role: 'assistant', text: 'Done.' }], 'claude_code', choice, t);
  assert.match(text, /^Here is a conversation I had with Claude Code\./);
  assert.match(text, /<conversation>\nYou:\nRename it\.\n\nClaude Code:\nDone\.\n<\/conversation>/);
  assert.ok(text.endsWith('Next: '));
  assert.match(handoffDraft([{ role: 'user', text: 'Hi' }], 'claude_code', { ...choice, branch: 'feature/x' }, t), /^Here is a conversation I had in another checkout\. This session works on branch feature\/x/);
});

test('with nothing said there is nothing to carry', () => {
  assert.equal(handoffDraft([], 'claude_code', { provider: 'codex', profileId: null, name: '', note: '' }, t), '');
});
