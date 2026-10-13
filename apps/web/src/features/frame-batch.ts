/**
 * Items that arrive faster than a screen can show them -- a reply streams
 * dozens of pieces a second -- applied together once per frame, so the view
 * is rebuilt and re-rendered once per frame rather than once per piece.
 * `flush` applies what is waiting at once, for an arrival that must not be
 * reordered behind the queue (a snapshot replaces what the queue builds on).
 */
export function createFrameBatch<T>(apply: (items: T[]) => void, schedule: (run: () => void) => unknown = defaultSchedule) {
  let queue: T[] = [], pending = false;
  const flush = () => {
    pending = false;
    if (!queue.length) return;
    const items = queue; queue = [];
    apply(items);
  };
  return {
    push(item: T) { queue.push(item); if (!pending) { pending = true; schedule(flush); } },
    flush,
    /** Drops what is waiting: the view it was for has gone. */
    clear() { queue = []; },
    get size() { return queue.length; },
  };
}

/** The next frame; a hidden page has none, so a short timer stands in. */
function defaultSchedule(run: () => void) {
  if (typeof document !== 'undefined' && document.hidden || typeof requestAnimationFrame !== 'function') return setTimeout(run, 100);
  return requestAnimationFrame(() => run());
}
