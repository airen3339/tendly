import type { AgentProviderKind, EndpointProfile } from "@tendly/protocol";
import { sharesHostConfig } from "./native-profiles";

/**
 * The endpoint profile form, as plain functions: what a save sends, and the
 * one-line summary each profile shows in the list.
 */

export interface ProfileForm {
  name: string; provider: AgentProviderKind; endpoint_url: string; api: string; model: string; effort: string;
  permission_mode: EndpointProfile["permission_mode"]; secret_ref: string; proxy_url: string;
}

/**
 * What saving an ordinary profile sends. Text is trimmed and an empty field
 * is null. An optional setting -- proxy, aliases, effort, chosen models, API
 * -- is sent when it has a value or the profile already had one, so clearing
 * it clears it while a profile that never had it is not given an empty one.
 * Only a client with a choice of API takes one.
 */
export function profilePayload(form: ProfileForm, previous: EndpointProfile | undefined, aliases: Record<string, string>, models: readonly string[], offersApi: boolean) {
  return {
    name: form.name.trim(), provider: form.provider, endpoint_url: form.endpoint_url.trim() || null, model: form.model.trim() || null,
    permission_mode: form.permission_mode, secret_ref: form.secret_ref.trim() || null,
    ...(form.proxy_url.trim() || previous?.proxy_url !== undefined ? { proxy_url: form.proxy_url.trim() || null } : {}),
    ...(Object.keys(aliases).length || previous?.model_aliases !== undefined ? { model_aliases: aliases } : {}),
    ...(form.effort.trim() || previous?.effort !== undefined ? { effort: form.effort.trim() || null } : {}),
    ...(models.length || previous?.models !== undefined ? { models: [...models] } : {}),
    ...(offersApi && (form.api || previous?.api) ? { api: form.api || null } : {}),
  };
}

/** A profile in one line: its model, the endpoint's host and a proxy if any; or, for a client's own configuration, whose sign-in it uses. */
export function profileSummary(profile: EndpointProfile, t: (text: string) => string): string {
  if (profile.native_config) return sharesHostConfig(profile.native_config) ? t("Host configuration · shared sign-in") : t("Isolated configuration · this account only");
  let host = "";
  try { host = profile.endpoint_url ? new URL(profile.endpoint_url).host : ""; } catch { host = profile.endpoint_url ?? ""; }
  return [profile.model || t("Native client default"), host || t("Official endpoint"), profile.proxy_url ? t("Proxy configured") : ""].filter(Boolean).join(" · ");
}
