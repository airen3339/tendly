import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chatEscapeAction, composerKeyAction, suggestionKey } from './chat-keyboard.ts';

test('composer shortcuts send on Enter, preserve newlines on Shift+Enter and ignore IME composition', () => {
  assert.equal(composerKeyAction({ key: 'Enter' }), 'send');
  assert.equal(composerKeyAction({ key: 'Enter', ctrlKey: true }), 'send');
  assert.equal(composerKeyAction({ key: 'Enter', metaKey: true }), 'send');
  assert.equal(composerKeyAction({ key: 'Enter', shiftKey: true }), 'newline');
  assert.equal(composerKeyAction({ key: 'Enter', altKey: true }), 'newline');
  assert.equal(composerKeyAction({ key: 'Enter', isComposing: true }), 'none');
  assert.equal(composerKeyAction({ key: 'Enter', keyCode: 229 }), 'none');
  assert.equal(composerKeyAction({ key: 'Enter', repeat: true }), 'suppress');
  assert.equal(composerKeyAction({ key: 'Escape' }), 'none');
});

const idle = { menuOpen: false, confirmEnd: false, confirmEndpoint: false, actionBusy: false, composerFocused: true, turnRunning: false, awaitingApproval: false, preview: false };

test('Escape closes menu/confirmation first, then interrupts only the active reply', () => {
  assert.equal(chatEscapeAction({ ...idle, menuOpen: true }), 'close-menu');
  assert.equal(chatEscapeAction({ ...idle, confirmEnd: true }), 'cancel-end');
  assert.equal(chatEscapeAction({ ...idle, confirmEndpoint: true }), 'cancel-endpoint');
  assert.equal(chatEscapeAction({ ...idle, turnRunning: true }), 'interrupt');
  assert.equal(chatEscapeAction({ ...idle, turnRunning: true, awaitingApproval: true }), 'blur');
  assert.equal(chatEscapeAction({ ...idle, preview: true, turnRunning: true }), 'blur');
  assert.equal(chatEscapeAction({ ...idle, actionBusy: true, turnRunning: true }), 'none');
  assert.equal(chatEscapeAction({ ...idle, composerFocused: false }), 'none');
});

test('the composer contract documents the outer focus ring and compact desktop typography', async () => {
  const { readFile } = await import('node:fs/promises');
  const source = await readFile(new URL('./ChatSessionPane.vue', import.meta.url), 'utf8');
  assert.match(source, /\.chat-composer:focus-within\{border-color:var\(--focus\)/);
  assert.match(source, /\.chat-composer>textarea:focus-visible\{outline:0/);
  assert.match(source, /\.chat-message :deep\(\.chat-markdown\)\{font-size:var\(--text-md\);line-height:1\.7\}/);
});

test('the composer uses compact desktop controls and restores touch targets on mobile', async () => {
  const { readFile } = await import('node:fs/promises');
  // A Windows checkout may carry CRLF; the rules below match line by line.
  const source = (await readFile(new URL('./ChatSessionPane.vue', import.meta.url), 'utf8')).replace(/\r\n/g, '\n');
  // The composer is centred under the thread (max-width 48rem); a narrow pane
  // gives it the pane's width less a small gutter.
  assert.match(source, /\.chat-composer\{width:calc\(100% - 24px\);margin:0 auto 8px;padding:8px 9px max\(5px,env\(safe-area-inset-bottom\)\);border-radius:var\(--radius-md\)\}/);
  // Endpoint and thinking depth are one shared chip control now, not a native
  // select, so their density contract lives with that primitive.
  const chip = await readFile(new URL('./ChipMenu.vue', import.meta.url), 'utf8');
  assert.match(chip, /\.chip-menu>summary\{[^}]*min-height:28px;[^}]*border-radius:var\(--radius-sm\)/);
  assert.match(chip, /@media\(pointer:coarse\)\{\.chip-menu>summary\{min-height:44px/);
  assert.match(source, /\.chat-send\{width:30px;height:30px;min-width:30px;min-height:0;border-radius:var\(--radius-md\)/);
  assert.match(source, /\.chat-composer \.chat-attach\{width:30px;height:30px;min-width:30px;min-height:0;border-radius:var\(--radius-md\)\}/);
  // Touch targets follow the pointing device, not the pane width: a narrow pane
  // on a desktop is still a mouse, and a phone needs 44px at any width.
  // Touch sizing lives in one coarse-pointer block, so a second one would
  // split the contract this asserts.
  const blocks = [...source.matchAll(/@media\(pointer:coarse\)\{/g)];
  assert.equal(blocks.length, 1, 'touch sizing stays in a single block');
  const coarse = /@media\(pointer:coarse\)\{[\s\S]*?\n\}/.exec(source)[0];
  for (const control of ['.chat-composer .chat-send', '.chat-composer .chat-attach']) {
    assert.ok(coarse.includes(`${control}{width:44px;height:44px;min-width:44px;min-height:44px`), control);
  }
  assert.match(coarse, /\.chat-attachments button\{width:44px;height:44px\}/);
  // The 16px input font belongs to the touch rule; it exists to stop iOS
  // zooming on focus, not because a pane is narrow. Only a real text field can
  // trigger that zoom, so the chip summaries are deliberately not included.
  assert.match(coarse, /\.chat-composer>textarea\{font-size:var\(--input-text\)\}/);
  const width = /@container\(max-width:480px\)\{\n(?:.*\n)*?\}/.exec(source)[0];
  assert.equal(/min-height:44px|width:44px/.test(width), false, width);
});

test('the working indicator actually animates rather than showing three still dots', async () => {
  const { readFile } = await import('node:fs/promises');
  const source = await readFile(new URL('./ChatSessionPane.vue', import.meta.url), 'utf8');
  // "Agent is working" was a static row for long enough to look frozen. The
  // turning mark is the client's own, so it also says which agent is thinking.
  assert.match(source, /class="chat-working-mark" :provider="session\.provider"/);
  assert.match(source, /\.chat-working-mark\{animation:chat-spin/);
  // Eased, not a constant sweep: a linear spin reads as a loading GIF, so the
  // mark surges and settles twice per turn instead.
  const spin = /@keyframes chat-spin\{[^}]*(?:\}[^@}]*)*?\}\}/.exec(source)[0];
  assert.ok(spin.split('transform:rotate').length - 1 >= 4, spin);
  assert.doesNotMatch(source, /animation:chat-spin [^;]*linear/);
  // The label is a sweep of light across the words, the way a running tool
  // row reads, rather than a fade that could pass for a stuck transition.
  assert.match(source, /\.chat-working-label\{[^}]*animation:chat-text-shimmer/);
  // Motion is opt-out, like every other animation in this pane.
  const reduced = /@media\(prefers-reduced-motion:reduce\)\{[^}]*\}[^@]*/.exec(source)[0];
  assert.match(reduced, /\.chat-working-mark,\.chat-working-label\{animation:none\}/);
});

test('an open suggestion list takes the arrows, Tab, Enter and Escape, and nothing else', () => {
  assert.equal(suggestionKey({ key: 'ArrowDown' }), 'next');
  assert.equal(suggestionKey({ key: 'ArrowUp' }), 'previous');
  assert.equal(suggestionKey({ key: 'Tab' }), 'choose');
  assert.equal(suggestionKey({ key: 'Enter' }), 'choose');
  assert.equal(suggestionKey({ key: 'Escape' }), 'dismiss');
  assert.equal(suggestionKey({ key: 'Enter', shiftKey: true }), undefined, 'Shift+Enter is still a new line');
  assert.equal(suggestionKey({ key: 'Enter', repeat: true }), undefined, 'a held Enter completes nothing');
  assert.equal(suggestionKey({ key: 'a' }), undefined);
});
