import { SUBAGENT_TOOLS, type ChatItem, type ToolItem } from './chat-model';
/** A stretch of tool calls the agent made one after another, shown as one card. */
export interface ToolRun { type: 'tool_run'; id: string; tools: ToolItem[] }
export type DisplayItem = Exclude<ChatItem, { type: 'tool' }> | ToolRun;

/**
 * Consecutive tool calls, folded into runs.
 *
 * Five `Bash` cards in a row said the same thing five times and pushed the
 * answer off the screen. A run is one card that says how many calls it holds
 * and opens to each of them. A lone call is still a run of one, so it keeps its
 * card and its place; a failed call closes the run it ends so it is never
 * folded away behind a summary that reads as fine.
 */
export function toolRuns(items: readonly ChatItem[]): DisplayItem[] {
  const out: DisplayItem[] = [];
  let run: ToolRun | undefined;
  for (const item of items) {
    if (item.type !== 'tool') { run = undefined; out.push(item); continue; }
    if (item.status === 'failed') { run = undefined; out.push({ type: 'tool_run', id: item.id, tools: [item] }); continue; }
    if (!run) { run = { type: 'tool_run', id: item.id, tools: [] }; out.push(run); }
    run.tools.push(item);
  }
  return out;
}

/** "Bash ×3 · Read ×2", in the order the tools were first used. */
export function toolRunNames(run: ToolRun): string {
  const counts = new Map<string, number>();
  for (const tool of run.tools) counts.set(tool.name, (counts.get(tool.name) ?? 0) + 1);
  return [...counts].map(([name, count]) => count > 1 ? `${name} ×${count}` : name).join(' · ');
}

export function toolRunStatus(run: ToolRun): 'running' | 'failed' | 'completed' {
  return run.tools.some(tool => tool.status === 'running') ? 'running' : run.tools.some(tool => tool.status === 'failed') ? 'failed' : 'completed';
}

// The collapsed summary shows where a subagent is now, not where it began.
export function lastLine(activity: string) {
  const lines = activity.split('\n').filter(line => line.trim() && line !== '…');
  const last = lines[lines.length - 1] ?? '';
  return last.length > 80 ? `${last.slice(0, 79)}…` : last;
}

/** A card's nested calls, depth first, each with how deep it sits. */
export function descendants(tool: ToolItem, depth = 1): Array<{ tool: ToolItem; depth: number }> {
  return (tool.children ?? []).flatMap(child => [{ tool: child, depth }, ...descendants(child, depth + 1)]);
}

/** Running, and started by the client that is still there to finish it. */
export const isLive = (tool: ToolItem) => tool.status === 'running' && !tool.ended;

/** Subagents still at work anywhere in the conversation, outermost first. */
export function runningSubagents(items: readonly ChatItem[]): ToolItem[] {
  const found: ToolItem[] = [];
  const visit = (tool: ToolItem) => { if (isLive(tool) && SUBAGENT_TOOLS.has(tool.name)) found.push(tool); tool.children?.forEach(visit); };
  for (const item of items) if (item.type === 'tool') visit(item);
  return found;
}

/** Running subagents not already inside another running one: each is a row of
 * its own, and what it started shows in its preview. A parent that finished
 * while its child runs on leaves the child a row of its own. */
export function subagentRoots(items: readonly ChatItem[]): ToolItem[] {
  const found: ToolItem[] = [];
  const visit = (tool: ToolItem) => { if (isLive(tool) && SUBAGENT_TOOLS.has(tool.name)) found.push(tool); else tool.children?.forEach(visit); };
  for (const item of items) if (item.type === 'tool') visit(item);
  return found;
}

/** Every tool card by id, nested ones included. */
export function toolsById(items: readonly ChatItem[]): Map<string, ToolItem> {
  const found = new Map<string, ToolItem>();
  const visit = (tool: ToolItem) => { found.set(tool.id, tool); tool.children?.forEach(visit); };
  for (const item of items) if (item.type === 'tool') visit(item);
  return found;
}

/** Long enough that a subagent thinking through a hard step is not called stuck. */
export const STALL_MS = 5 * 60_000;
/** A running card the client has said nothing about for a while. Only cards
 * that report progress at all carry `at`; the rest are never called stalled. */
export function stalledFor(tool: ToolItem, now: number): number | undefined {
  if (!isLive(tool) || tool.at === undefined) return;
  const quiet = now - tool.at;
  return quiet >= STALL_MS ? quiet : undefined;
}

/** "3.4k", "12", "1.2M": counts the way a glance reads them. */
export function compactCount(value: number) {
  if (value < 1000) return String(value);
  if (value < 1_000_000) return `${(value / 1000).toFixed(value < 10_000 ? 1 : 0).replace(/\.0$/, '')}k`;
  return `${(value / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`;
}
