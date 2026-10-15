/**
 * Carrying a conversation over to a different agent.
 *
 * Claude Code and Codex keep their histories in formats neither can read, so a
 * conversation cannot be moved between them as it is. What can be moved is what
 * was said: this writes the visible conversation out as text for the other
 * client's first message. It is a transcript, not a resumed session -- the new
 * client sees the words, not the tool results or files the old one had open --
 * and the text is placed in the composer rather than sent, so it is read and
 * finished by the user first.
 */
import type { EndpointProfile, ProviderKind } from '@tendly/protocol';
import { AGENT_CLIENTS, providerLabel } from './clients';

type Translate = (text: string, values?: Record<string, string | number>) => string;

/** A message the transcript is made from. Tool cards and approvals are left
 * out: their output is often large, and the new agent can look again itself. */
export interface HandoffMessage { role: 'user' | 'assistant'; text: string }

/** The server refuses a message over 32768 bytes; the rest of this budget is
 * left for the framing and for whatever the user adds after it. */
export const HANDOFF_TRANSCRIPT_BYTES = 26 * 1024;

const encoder = new TextEncoder();
const bytes = (text: string) => encoder.encode(text).byteLength;

/** The end of `text` that fits in `budget` bytes, cut on a character boundary. */
function tail(text: string, budget: number): string {
  if (bytes(text) <= budget) return text;
  const chars = [...text];
  let used = 0, start = chars.length;
  while (start > 0) {
    const size = bytes(chars[start - 1]);
    if (used + size > budget) break;
    used += size; start--;
  }
  return chars.slice(start).join('');
}

export interface HandoffLabels {
  /** Name for the user's turns, e.g. "You". */
  user: string;
  /** Name for the old agent's turns, e.g. "Claude Code". */
  assistant: string;
  /** Said once, in place of whatever did not fit. */
  omitted: string;
}

/**
 * The most recent messages that fit in `budget`, oldest first.
 *
 * Recent turns are the ones the next agent needs, so it is the start that is
 * dropped. A single message larger than everything allowed keeps its end.
 */
export function handoffTranscript(messages: readonly HandoffMessage[], labels: HandoffLabels, budget = HANDOFF_TRANSCRIPT_BYTES): { text: string; omitted: boolean } {
  const blocks = messages
    .filter(message => message.text.trim())
    .map(message => `${message.role === 'user' ? labels.user : labels.assistant}:\n${message.text.trim()}`);
  const kept: string[] = [];
  let used = 0;
  for (let index = blocks.length - 1; index >= 0; index--) {
    const size = bytes(blocks[index]) + 2;
    if (used + size > budget) {
      if (!kept.length) kept.unshift(tail(blocks[index], budget));
      break;
    }
    kept.unshift(blocks[index]); used += size;
  }
  const omitted = kept.length < blocks.length || (kept.length === 1 && kept[0] !== blocks[blocks.length - 1]);
  return { text: (omitted ? `(${labels.omitted})\n\n` : '') + kept.join('\n\n'), omitted };
}

/** Sessions just opened by a handoff, shared across panes: the new session
 * mounts in a different pane from the one that made it. That pane puts the
 * caret after the written-out conversation, where the next instruction goes. */
export const handoffArrivals = new Set<string>();

/**
 * What choosing each configuration actually means.
 *
 * "Isolated" read as the careful, better choice, when what it does is start
 * the client with an empty home: no sign-in, none of the host's settings. The
 * menu is the only place that choice is made, so it is where that is said.
 */
export function profileNote(profile: EndpointProfile, t: Translate): string {
  if (profile.native_config) return t('Uses the sign-in and settings in {path}', { path: profile.native_config.config_dir });
  if (profile.endpoint_url) return t('Custom endpoint · {url}', { url: profile.endpoint_url });
  return t('Custom endpoint profile');
}

/** Another client, and the configuration it would start with. */
export interface HandoffChoice { provider: ProviderKind; profileId: string | null; name: string; note: string; workspaceId?: string; branch?: string }

/**
 * Where a conversation with `provider` can be carried: every other client,
 * under each of its profiles and with a fresh isolated home. Offered with an
 * empty conversation too: then it is simply the same work in another client.
 * A terminal has no conversation to carry.
 */
export function handoffChoices(provider: ProviderKind, profiles: readonly EndpointProfile[], t: Translate): HandoffChoice[] {
  if (provider === 'terminal') return [];
  return AGENT_CLIENTS.filter(other => other !== provider).flatMap(other => [
    ...profiles.filter(profile => profile.provider === other).map(profile => ({ provider: other, profileId: profile.id, name: profile.name, note: profileNote(profile, t) })),
    { provider: other, profileId: null, name: t('Native · isolated configuration'), note: t('A fresh, empty client home: no host sign-in or settings, so it may ask you to log in.') },
  ]);
}

/** The new session's title: the old one, with the branch or client it moved to. */
export function handoffTitle(title: string, choice: HandoffChoice, t: Translate): string {
  return t('{title} · {provider}', { title, provider: choice.branch ?? providerLabel(choice.provider) });
}

/**
 * The new session's first message, left in its composer: the conversation as
 * a transcript, and a prompt for what to do next. Nothing said yet means
 * nothing to carry, and the new session starts clean.
 */
export function handoffDraft(messages: readonly HandoffMessage[], from: ProviderKind, choice: HandoffChoice, t: Translate): string {
  if (!messages.length) return '';
  const transcript = handoffTranscript(messages, { user: t('You'), assistant: providerLabel(from), omitted: t('Earlier messages did not fit and were left out') });
  const opening = choice.branch
    ? t('Here is a conversation I had in another checkout. This session works on branch {branch} in its own worktree; continue the work here, and do not redo what is already done.', { branch: choice.branch })
    : t('Here is a conversation I had with {agent}. Pick up the work where it left off: read it first, and do not redo what is already done.', { agent: providerLabel(from) });
  return opening + '\n\n<conversation>\n' + transcript.text + '\n</conversation>\n\n' + t('Next:') + ' ';
}
