import type { AgentProviderKind, ModelCatalog } from '@tendly/protocol';
import { agentClient } from './clients';

/** The values accepted by both native clients' configuration surfaces. */
export const REASONING_EFFORTS = ['low', 'medium', 'high', 'xhigh', 'max'] as const;
export type ReasoningEffort = typeof REASONING_EFFORTS[number];

/**
 * Claude Code exposes --effort as a client option, so it gets every level
 * (`effortLadder` in clients.ts). Codex is model-specific and must use the
 * levels advertised by its model/list response; an empty catalog
 * deliberately produces no guessed choices. Custom endpoints may omit capability
 * metadata, so allow a manual choice using the client vocabulary.
 */
export function modelEfforts(
  provider: AgentProviderKind,
  model: string | undefined,
  catalog: ModelCatalog | undefined,
  current?: string | null,
  customEndpoint = false,
): string[] {
  const advertised = model ? catalog?.models.find(entry => entry.id === model)?.efforts ?? [] : [];
  const values = (customEndpoint || agentClient(provider)?.effortLadder) && !advertised.length
    ? (provider === 'pi' ? ['low', 'medium', 'high'] : [...REASONING_EFFORTS])
    : advertised.filter(value => (REASONING_EFFORTS as readonly string[]).includes(value));
  if (current && (REASONING_EFFORTS as readonly string[]).includes(current) && !values.includes(current)) values.push(current);
  return [...new Set(values)];
}

export function effortLabel(value: string): string {
  return value === 'xhigh' ? 'X-high' : value[0].toUpperCase() + value.slice(1);
}
