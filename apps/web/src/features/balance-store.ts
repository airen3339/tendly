import { reactive } from 'vue';
import { ApiError, errorMessage, json, request } from './api';
import { backendCapabilities } from './backend-capabilities';
import { shouldAutoRefresh, type BalanceView, type EndpointBalance } from './balance-model';
import type { AccountView } from './account-ui';

/**
 * One copy of the configured endpoint balances for the whole page, so the
 * message box, the endpoint menu and the Usage page never disagree about a
 * figure, and a refresh in one place shows everywhere.
 */
export const balances = reactive({ rows: [] as EndpointBalance[], busy: [] as string[], error: '' });
const lastAttempt = new Map<string, number>();
let loading: Promise<void> | undefined;

export function balanceFor(profileId?: string | null): EndpointBalance | undefined {
  return profileId ? balances.rows.find(row => row.id === profileId) : undefined;
}

export function loadBalances(): Promise<void> {
  if (!backendCapabilities.customBalances) return Promise.resolve();
  loading ??= request<EndpointBalance[]>('/endpoint-balances')
    .then(rows => { balances.rows = rows; balances.error = ''; })
    .catch(cause => { if (!(cause instanceof ApiError && cause.status === 404)) balances.error = errorMessage(cause); })
    .finally(() => { loading = undefined; });
  return loading;
}

export async function refreshBalance(profileId: string): Promise<void> {
  if (balances.busy.includes(profileId)) return;
  balances.busy.push(profileId);
  lastAttempt.set(profileId, Date.now());
  try {
    const view = await request<BalanceView>(`/endpoint-profiles/${encodeURIComponent(profileId)}/balance/query`, json('POST'));
    applyView(profileId, view);
  } catch (cause) {
    const row = balanceFor(profileId);
    if (row) { row.error = errorMessage(cause); row.stale = true; }
  } finally {
    balances.busy = balances.busy.filter(id => id !== profileId);
  }
}

/** Queries again only when the figure is old and this page has not just asked; the server keeps its own cooldown too. */
export function refreshBalanceIfStale(profileId?: string | null): void {
  const row = balanceFor(profileId);
  if (row && shouldAutoRefresh(row, lastAttempt.get(row.id), Date.now())) void refreshBalance(row.id);
  if (accountFor(profileId) && Date.now() - accountsReadAt > 60_000) void loadAccountQuotas();
}

/**
 * Official accounts by the endpoint profile each one signs in through, so an
 * endpoint chip can show the account's allowance the way it shows a custom
 * balance. Reading the list asks Tendly for what it last heard, never the
 * provider, so it is cheap to read again after a turn.
 */
export const accountQuotas = reactive({ byProfile: {} as Record<string, AccountView> });
let accountsReadAt = 0, accountsLoading: Promise<void> | undefined;

export function accountFor(profileId?: string | null): AccountView | undefined {
  return profileId ? accountQuotas.byProfile[profileId] : undefined;
}

export function loadAccountQuotas(): Promise<void> {
  if (!backendCapabilities.accounts) return Promise.resolve();
  accountsReadAt = Date.now();
  accountsLoading ??= request<AccountView[]>('/accounts')
    .then(items => { accountQuotas.byProfile = Object.fromEntries(items.map(account => [account.profile_id, account])); })
    // The chips simply show no allowance; the accounts dialog reports the failure.
    .catch(() => {})
    .finally(() => { accountsLoading = undefined; });
  return accountsLoading;
}

/** The editor saved or queried a configuration: take its view, or drop the row when the query was disabled. */
export function applyView(profileId: string, view: BalanceView, name?: string): void {
  const index = balances.rows.findIndex(row => row.id === profileId);
  if (!view.config) { if (index >= 0) balances.rows.splice(index, 1); return; }
  const next = { id: profileId, name: name ?? balances.rows[index]?.name ?? '', reading: view.reading, error: view.error, stale: view.stale, attempted_at: view.attempted_at };
  if (index >= 0) Object.assign(balances.rows[index], next); else void loadBalances();
}
