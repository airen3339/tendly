/**
 * Which library draws the terminal.
 *
 * Trial switch: xterm.js paints a canvas and moves it a whole row at a time;
 * wterm lays the rows out as page text, so the browser scrolls them by the
 * pixel. `?terminal=wterm` (or `xterm`) in the address picks one and is
 * remembered for this browser.
 */
export type TerminalEngine = "xterm" | "wterm";
const KEY = "tendly.terminalEngine";

export function terminalEngine(): TerminalEngine {
  try {
    const asked = new URL(window.location.href).searchParams.get("terminal");
    if (asked === "wterm" || asked === "xterm") localStorage.setItem(KEY, asked);
    return localStorage.getItem(KEY) === "wterm" ? "wterm" : "xterm";
  } catch { return "xterm"; }
}

/**
 * Ghostty logs each sequence it ignores, such as the icon title many prompts
 * set on every line, straight to the console with no way to turn it off.
 */
let ghosttyQuiet = false;
export function quietGhostty() {
  if (ghosttyQuiet) return;
  ghosttyQuiet = true;
  const log = console.log;
  console.log = (...args: unknown[]) => { if (args[0] !== "[ghostty-vt]") log.apply(console, args); };
}
