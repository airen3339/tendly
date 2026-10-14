import { json, request } from "./api";
import { tokenFromHash } from "./tailnet";

/**
 * A sign-in link from Settings → Remote access carries the token in its
 * fragment, which the browser never sends to a server.
 *
 * This runs as main.ts's first import, before the router reads the address:
 * the router writes the address it started with back into history, so taking
 * the token out any later left it in the address bar, history and bookmarks.
 * The app mounts once the sign-in is settled, so nothing asks the API first
 * and meets a 401.
 */
export type LinkSignIn = "none" | "signed-in" | "failed";
function begin(): Promise<LinkSignIn> {
  const token = typeof location === "undefined" ? null : tokenFromHash(location.hash);
  if (!token) return Promise.resolve("none");
  history.replaceState(history.state, "", location.pathname + location.search);
  return request("/auth", json("POST", { token })).then(() => "signed-in" as const, () => "failed" as const);
}
export const linkSignIn = begin();
