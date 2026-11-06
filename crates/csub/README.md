# csub

`csub` owns a pool of Claude subscriptions. It runs as one local daemon that
keeps usage fresh, refreshes OAuth grants, and places Claude Code requests on
an account that still has allowance. Pool logic lives in
`rayline-subscriptions`; the Rayline CLI reuses the same library.

## Register accounts

The registry is `~/.config/rayline/subscriptions.json` (schema 1, mode 0600).
It holds paths and policy only. Tokens stay in each Claude profile's own
credential store.

```bash
csub add af --claude-config-dir ~/.claude-af
csub add mx --claude-config-dir ~/.claude-mx
csub list
```

## Run the daemon

```bash
csub service install      # launchd job ai.rayline.csub running `csub serve`
csub service status
csub status               # live allowance and placement
csub reload               # reread every credential source after a re-login
```

`csub serve` takes an exclusive lock at
`~/.rayline/subscriptions/pool-<id>.lock`. A second owner exits with status 75,
so launchd's restarts never start a second refresher. It also refuses to start
while the legacy `rld proxy --subscription-pool` daemon is running.

## Route Claude Code through the pool

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:20816/l/$(uuidgen) claude
```

`POST /v1/messages` gets a pool account's bearer token. A request is moved to
another account only when it is rejected before streaming starts, for quota,
entitlement or credentials. Every other path is forwarded unchanged with the
client's own credentials. No CA certificate or `HTTPS_PROXY` is needed.

## HTTP API (127.0.0.1 only)

| Route | Purpose |
|---|---|
| `GET /healthz` | `{"ok":true,"service":"csub","pid":…,"pool":…}` |
| `GET /v1/subscriptions/status` | `PoolRuntimeStatus` JSON; check `schema` (1; missing means 0, the legacy daemon) |
| `POST /v1/subscriptions/reload` | Reread credential sources; requires `content-type: application/json` |
| `/l/<launch-id>/…` | Gateway |
