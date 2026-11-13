# Rayline Agent Notes

`CLAUDE.md` is a symlink to this file. Keep rules short, current, and
repo-specific.

## Crate Boundaries

- `rayline-cli`: user commands and CLI UX.
- `rayline-daemon`: `rld hybrid`, the local hybrid gateway.
- `rayline-local-router`: shared helpers for the hybrid gateway.
- `rayline-hf`, `rayline-tokencount`, `rayline-metrics`: model cache, token
  counting and request metrics for hybrid mode.

Keep protocol and routing logic in shared crates. Do not duplicate it in CLI
handlers. Prefer focused modules over growing the existing large CLI files.

## CLI Contracts

- `docs/spec.md` is the contract for every command, option, config key, env
  var and file. Implement exactly what it lists. Add nothing it does not list.
- Fail fast: validate config, env overrides and flags once at load and refuse
  with an error naming the key or flag. Code after load trusts the parsed
  config: no checks, defaults or fallbacks at call sites.
- No fallbacks, aliases, alternative spellings, migrations or compatibility
  code. One name per key and flag, one value per meaning.
- No hard-coded model ids, context windows, output limits or prices.
- Code and comments describe what the code does now, not what it replaced.

## Commands

```bash
cargo build --workspace --locked
cargo build --release -p rayline-cli -p rayline-daemon --locked
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 test --workspace --locked -- --test-threads=1
cargo +1.88.0 clippy --workspace --all-targets --locked -- -D warnings
cargo audit --file Cargo.lock
cargo deny --locked check advisories bans licenses sources
git diff --check
```

Use the pinned Rust toolchain. Run targeted checks while iterating and the
relevant full gate before handoff.

## Security And Privacy

- Never commit secrets, API keys, auth headers, private prompts, customer data,
  generated certs, private keys, or unsanitized logs.
- Treat `rls_`, `rlr_`, `rlk-`, provider keys, `RAYLINE_ROUTER_API_KEY`, and
  Claude/Anthropic auth env vars as secrets.
- Changes touching key storage, harness launch environments, hybrid routing,
  install/update scripts, release workflows, or auth storage need extra review.
- Do not log full prompts or credentials in normal output, tests, or diagnostics.

## Rust Rules

- Keep `Cargo.lock` committed and use `--locked`.
- Do not relax MSRV, formatting, clippy, or workspace lint policy casually.
- Avoid `unwrap`, `expect`, and panics on user input, OS/filesystem state,
  networking, TLS, config, provider responses, or subprocess behavior.
- `expect` is acceptable for proven internal invariants, such as poisoned locks.
- Every `unsafe` block needs a nearby `SAFETY:` comment explaining the invariant.
- Avoid holding locks across `.await`.

## Tests

- Prefer local fake services, `127.0.0.1:0`, and behavior-level assertions.
- Default tests must not require live provider credentials or external services.
- Update docs when public CLI flags, env vars, routing behavior, install/update
  behavior, or release behavior changes.

## Dependencies And Release

- Workspace crates stay `publish = false` until package publishing is explicitly
  reviewed.
- Do not add git dependencies or unknown registries without maintainer review.
- Preserve pinned GitHub Actions and minimal workflow permissions.
- Release assets must remain binary-first and verifiable with `SHA256SUMS`.
- Homebrew packaging should use archive assets, not bare binaries.
