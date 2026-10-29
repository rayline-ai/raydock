# Claude subscription pool (csub)

`csub` runs one local daemon that pools several Claude subscriptions. It keeps
usage fresh, refreshes OAuth grants, and places each Claude Code request on an
account that still has allowance. Raydock reuses it as `raydock subscriptions`
and `raydock claude --subscription-pool`. See `crates/csub/README.md` for the
daemon's HTTP API.

## Set up

```bash
raydock subscriptions add work --claude-config-dir ~/.claude-work
raydock subscriptions add personal --claude-config-dir ~/.claude-personal
raydock subscriptions service install   # launchd job ai.rayline.csub (macOS)
raydock subscriptions status
```

The registry is `~/.config/rayline/subscriptions.json`. It holds paths and
policy only; tokens stay in each profile's Claude credential store. `csub
serve` owns the pool through `~/.rayline/subscriptions/pool-<id>.lock`, so a
second daemon refuses to start. Two processes refreshing the same grants would
invalidate each other.

## Run Claude Code on the pool

```bash
raydock claude --subscription-pool            # pool "default"
raydock claude --subscription-pool=work -- -p "hello"
```

This runs `claude` with
`ANTHROPIC_BASE_URL=http://127.0.0.1:20816/l/<launch-id>`. No CA, `HTTPS_PROXY`
or Rayline router is involved, so router flags (`--local`, `--via`, `--route`,
`--isolated`, …) are rejected alongside it. All other arguments go to `claude`
unchanged, except `--settings`, `--managed-settings`, `--project-config-root`
and `--deep-link-cwd-b64`, which are refused: the launcher passes its own
`--settings` so the gateway URL wins over user and project settings, and checks
settings for the directory it starts in. A profile signed in to a Cloud
gateway (`claude auth logout` clears it) is refused too, since that sign-in
outranks `ANTHROPIC_BASE_URL`.

The launch is refused when a settings `env` block (managed, user, project,
local, or the global `~/.claude.json`) sets an API key, a cloud provider switch
(Bedrock, Vertex, Foundry, …) or a provider credential; when managed settings
(or, for `--bg`, any settings) set `ANTHROPIC_BASE_URL`; or when managed
settings use a `policyHelper`. The same variables are removed from the inherited
environment. A `--bg`, `agents` or background-session (`attach`, `respawn`, …) launch is
also refused while Claude Code's background supervisor runs off the pool (or
carries a withheld variable); stop it with
`claude daemon stop --any` first.

A request moves to another account only when Anthropic rejects it before
streaming starts, for quota, entitlement or credentials. Generic rate limits,
overloads, network failures and partially streamed responses are never
replayed.

`raydock subscriptions` accepts every `csub` command (`add`, `remove`, `list`,
`status`, `reload`, `serve`, `service`, `claude`).
