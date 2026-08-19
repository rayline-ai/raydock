# Rayline Agent Notes

`CLAUDE.md` is a symlink to this file. Keep rules short, current, and
repo-specific.

## Crate Boundaries

- `rayline-cli`: user commands and CLI UX.
- `rayline-daemon`: `rld` process orchestration.
- `rayline-proxy`: proxying, TLS interception, CA handling, request routing.
- `rayline-injector`: process/environment injection.
- `rayline-adapter`: adapter protocol translation.
- `rayline-local-router`: static routing policy and endpoint selection.
- `rayline-hf`, `rayline-llama`, `rayline-metrics`: model/cache/runtime support.

Keep protocol and routing logic in shared crates. Do not duplicate it in CLI
handlers. Prefer focused modules over growing the existing large CLI/proxy files.

## CLI Contracts

- Use the current routing flags in new docs, tests, and examples:
  `--local`, `--via proxy|env`, and `--route all|subagents|none`.
- Treat `--local-router`, `--no-proxy`, and `--routing-mode ...` as deprecated
  compatibility aliases only.
- `--local` means local static routing without hosted auth.
- `--via env` is cloud-only and must not be combined with local inference or
  selective subagent routing.
- Local routing defaults to `--route subagents`; cloud routing defaults to
  `--route all`.
- Use `--router-config-path` when `routes.subagents` is meant to constrain the
  transparent proxy allowlist. Do not rely on `RAYLINE_ROUTER_CONFIG` alone for
  that allowlist.
- `rayline top` should work for proxy-mode launches when metrics can bind.
  Metrics bind failures are best-effort and must not break proxy data flow.

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
- Changes touching TLS CA handling, key storage, process injection, proxy
  routing, install/update scripts, release workflows, or auth storage need extra
  review.
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
- Keep the live Claude Code proxy test ignored; run it explicitly only for that
  integration path:
  `CLAUDE_BIN=/path/to/claude cargo test -p rayline-proxy --test it_claude_live -- --ignored --nocapture`
- Update docs when public CLI flags, env vars, routing behavior, install/update
  behavior, or release behavior changes.

## Dependencies And Release

- Workspace crates stay `publish = false` until package publishing is explicitly
  reviewed.
- Do not add git dependencies or unknown registries without maintainer review.
- Preserve pinned GitHub Actions and minimal workflow permissions.
- Release assets must remain binary-first and verifiable with `SHA256SUMS`.
- Homebrew packaging should use archive assets, not bare binaries.
