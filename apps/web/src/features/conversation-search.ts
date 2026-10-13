import type { DisplayItem } from './tool-runs';
import type { ToolItem } from './chat-model';

/** One place a search found, as the server reports it. */
export interface SearchHit { seq: number; kind: 'message' | 'tool'; id: string; snippet: string }
export interface SearchResult { hits: SearchHit[]; more: boolean }

export function parseSearchResult(value: unknown): SearchResult {
  if (!value || typeof value !== 'object') throw Error('Invalid search response.');
  const v = value as Record<string, unknown>;
  if (!Array.isArray(v.hits) || typeof v.more !== 'boolean') throw Error('Invalid search response.');
  const hits = v.hits.map(hit => {
    const h = hit as Record<string, unknown>;
    if (!h || typeof h !== 'object' || typeof h.seq !== 'number' || !Number.isSafeInteger(h.seq) || (h.kind !== 'message' && h.kind !== 'tool') || typeof h.id !== 'string' || typeof h.snippet !== 'string') throw Error('Invalid search response.');
    return { seq: h.seq, kind: h.kind, id: h.id, snippet: h.snippet } as SearchHit;
  });
  return { hits, more: v.more };
}

/**
 * The row a hit is shown in: a message is its own row; a call sits in a run
 * of calls, possibly nested inside a subagent's. `path` names the calls to
 * open, outermost first, for one shown folded inside others.
 */
export function rowForHit(rows: readonly DisplayItem[], hit: SearchHit): { index: number; path: string[] } | undefined {
  for (let index = 0; index < rows.length; index++) {
    const row = rows[index]!;
    if (hit.kind === 'message' && row.type === 'message' && row.id === hit.id) return { index, path: [] };
    if (hit.kind === 'tool' && row.type === 'tool_run') {
      for (const tool of row.tools) { const path = pathTo(tool, hit.id); if (path) return { index, path }; }
    }
  }
  return undefined;
}
function pathTo(tool: ToolItem, id: string): string[] | undefined {
  if (tool.id === id) return [tool.id];
  for (const child of tool.children ?? []) { const path = pathTo(child, id); if (path) return [tool.id, ...path]; }
  return undefined;
}

/** The next hit to show, stepping through them in either direction and around. */
export function stepHit(count: number, current: number, direction: 1 | -1): number {
  if (!count) return -1;
  if (current < 0) return direction === 1 ? 0 : count - 1;
  return (current + direction + count) % count;
}

/** Where `query` appears in `text`, case aside for Latin letters, as [start, end) offsets. */
export function occurrences(text: string, query: string): Array<[number, number]> {
  const needle = query.trim().toLowerCase();
  if (!needle) return [];
  const haystack = text.toLowerCase(), found: Array<[number, number]> = [];
  // Lower-casing keeps lengths for the scripts searched here; offsets still line up.
  if (haystack.length !== text.length) return found;
  for (let at = haystack.indexOf(needle); at !== -1; at = haystack.indexOf(needle, at + needle.length)) found.push([at, at + needle.length]);
  return found;
}
