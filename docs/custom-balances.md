# Custom endpoint balances

In **Settings → Endpoint profiles**, save or select a profile and open **Custom balance query**. Official accounts need none: their endpoint chip and menu entry show the remaining share of the account's tightest window (5-hour or weekly) from the figures Tendly already has, and the editor is not offered for them. Choose:

- **Fiat balance**: a three-letter currency code, such as `USD`, `CNY` or `EUR`. Values stay in the original currency; Tendly does not convert currencies or add different currencies together. A negative amount can represent debt.
- **Remaining percentage**: a number from `0` to `100`, always representing what is left.
- **Disabled**: remove the query and its cached reading.

Set the balance API URL and where the key goes: the `Authorization: Bearer` or `x-api-key` header, any header whose value contains `{key}` (for example `X-Token: Token {key}`), a query parameter such as `?api_key=`, or no authentication. Tendly fills the key in on the server; scripts never see it, cannot set that header, and a query parameter of the same name in a script URL is replaced. The query uses the saved endpoint key unless you provide another saved `env:TENDLY_SECRET_…` reference. Save a separate key through `/api/secrets/{name}` if needed. Do not paste credentials into JavaScript. Changing a referenced key affects the next query. The endpoint's saved proxy setting also applies.

Balance configuration is saved independently of the profile's model settings. Existing sessions and official subscription percentages are unaffected. **Test balance script** runs the current draft and shows a preview without saving the configuration or replacing the cached balance. **Refresh saved balance** runs the saved script. The balance shows where it is spent: on the endpoint chip in each session's message box, beside every endpoint in that chip's menu (which also has **Query this endpoint's balance again**), and as cards at the top of the Usage page, each with a refresh button. Low readings change colour: a remaining percentage at 25% or less warns and at 10% or less is red; money at or below zero is red. There is no background polling. A session queries its endpoint again when a turn ends, and when it first shows an endpoint with a query, but only if the reading is over a minute old and that page has not asked within the minute. Repeated queries share a 15-second cooldown; readings older than five minutes are marked outdated. Failed queries keep the previous reading with an error and outdated marker. Changing configuration clears the old reading.

## Script contract

Enter an **async function body**, not a function declaration. The editor highlights JavaScript, completes `ctx.request()`, and offers the examples below under **Insert an example**. Return exactly the balance value in an object:

```js
const data = await ctx.request();
return { value: data.balance };
```

Integers and decimal numbers are accepted. For money, decimal strings preserve precision, for example `{ value: "123.4500" }`. Values have at most 18 integer digits and 12 fractional digits; exponent strings, missing values, booleans and non-finite numbers are rejected. The server assigns the unit and timestamp from the configuration, rather than trusting scripts to change them.

Examples:

```js
// API reports cents. Return exact decimal text without floating-point rounding.
const data = await ctx.request();
const cents = BigInt(data.balance_cents);
const absolute = cents < 0n ? -cents : cents;
return { value: `${cents < 0n ? '-' : ''}${absolute / 100n}.${String(absolute % 100n).padStart(2, '0')}` };
```

```js
// The endpoint reports percentage used.
const data = await ctx.request();
return { value: 100 - data.used_percent };
```

```js
// Calculate remaining percentage from total and remaining quotas.
const data = await ctx.request();
if (!(data.total > 0)) throw new Error('No usable quota total');
return { value: (100 * data.remaining / data.total).toFixed(2) };
```

`await ctx.request(options?)` returns parsed JSON. With no options it sends GET to the configured query URL. Options:

```js
const data = await ctx.request({
  url: '/account/balance', // resolved against query_url; must remain on the same origin
  method: 'POST',         // GET (default) or POST
  headers: { Accept: 'application/json' },
  body: { account: 'example' } // sent as JSON
});
return { value: data.available };
```

The server adds authentication; scripts cannot set Host, Cookie, connection headers, proxy credentials, Authorization, x-api-key, or the header configured to carry the key. Redirects are not followed. The sandbox has no Node.js APIs, filesystem, process environment, module loader, timers or ambient `fetch`. It uses a fresh QuickJS runtime with a 16 MiB memory limit, a 10-second execution/request deadline, at most four requests, and at most 1 MiB per response. Error responses and script exception text are not returned to callers; only sanitized diagnostics and validated numeric values leave the runner.

## HTTP API

All routes use the normal Tendly browser authentication:

- `GET /api/endpoint-balances`: configured endpoint names and cached results, without scripts.
- `GET /api/endpoint-profiles/{id}/balance`: configuration and cached result.
- `PUT /api/endpoint-profiles/{id}/balance`: save the following object, or `null` to disable.
- `POST /api/endpoint-profiles/{id}/balance/test`: run a draft configuration; no saved result is changed.
- `POST /api/endpoint-profiles/{id}/balance/query`: run the saved configuration and update its cached result.

```json
{
  "unit": { "kind": "money", "currency": "CNY" },
  "query_url": "https://api.example.com/account/balance",
  "auth": { "kind": "header", "name": "Authorization", "value": "Bearer {key}" },
  "secret_ref": null,
  "script": "const data = await ctx.request(); return { value: data.balance };"
}
```

`auth` is `{ "kind": "none" }`, `{ "kind": "header", "name": "X-Token", "value": "Token {key}" }` (the value holds `{key}` exactly once, on one line), or `{ "kind": "query", "name": "api_key" }`. Percentage uses `"unit": { "kind": "percentage" }` without a currency field. A successful reading contains `value` as decimal text, `unit`, and `checked_at`. The saved view also contains `attempted_at`, `error`, and `stale`.

## MCP

`tendly_balance` exposes `get`, `configure`, `test`, and `query` actions with an endpoint profile `id` from `tendly_list`.

`configure` takes `config` in the shape above (or `null`). It shows the destination, authentication reference, unit and full script for review in Tendly before saving. Keys stay in Tendly's secret storage, not the tool call. `test` executes only an already saved configuration; it cannot execute an unreviewed script supplied by the caller. `query` returns and caches the reading. The same validation and execution limits apply to browser and MCP requests.
