# Security Policy

## Supported Versions

Raydock, the command-line client in the broader Rayline ecosystem, is
under active pre-release development. Only the latest release and `main` are
supported; security fixes are applied there.

## Reporting a Vulnerability

Please do not open public issues for security vulnerabilities. Public reports
can expose users before a fix is available.

Report privately using GitHub Private Vulnerability Reporting:

1. Go to the Security tab of this repository.
2. Click Report a vulnerability.
3. Provide a clear description, affected version, and reproduction steps.

If you cannot use GitHub Private Vulnerability Reporting, email
[security@rayline.ai](mailto:security@rayline.ai).

When reporting, please include:

- the affected version and operating system;
- a description of the issue and its impact;
- step-by-step reproduction;
- any relevant logs with secrets and tokens removed.

## What to Expect

We aim to acknowledge reports promptly, confirm and assess the issue, and keep
you informed as we work on a fix and coordinate disclosure.

## Trust Model

This section documents what Rayline's local security mechanisms protect against,
what they do not protect against, and what residual risks remain.

### Threat boundary

**Loopback is not a security boundary on a single-user machine.** Any process
running as the same OS user can connect to Rayline's local ports, read the
router API key from a harness's process environment, or read key material from
the filesystem (subject to file permissions). Rayline's local protections are
defense-in-depth measures — they are not a hard isolation boundary.

For strong isolation (e.g., when running untrusted local code alongside
Rayline), use OS-level isolation: a separate OS user account, a container, or
a virtual machine.

### Router API key

The router API key authenticates harness sessions to the Rayline router. It is:

- **Revocable server-side**: keys are minted via `POST /v1/keys` and revoked
  via `POST /v1/auth/cli/revoke` (see `crates/rayline-cli/src/status.rs`,
  functions `mint_router_key_at` and `revoke_rayline_session`).
- **Withheld from harness environments**: Claude Code, Codex and the other
  harness spawns remove `RAYLINE_ROUTER_API_KEY` from the inherited environment
  (`crates/rayline-cli/src/codex.rs`, `withhold_router_key`). Claude Code gets the key only as
  `ANTHROPIC_AUTH_TOKEN`; Codex reads it from a 0600 `config.toml` in its
  Rayline `CODEX_HOME`, never from its arguments or environment.
- **Residual risk**: the key remains readable in Claude Code's process
  environment via `/proc/<pid>/environ` on Linux (or equivalently via
  `ps eww <pid>` / `proc_pidinfo` on macOS), and in Codex's `config.toml`, by
  any process running as the same UID. This is a known residual. Same-UID
  isolation requires OS-level controls.

### Recommended isolation for untrusted workloads

If you run untrusted local code alongside Rayline (e.g., MCP servers from
unknown sources, arbitrary shell commands), use OS-level isolation:

- Run Rayline under a dedicated OS user account, or
- Use a container or VM that separates Rayline from untrusted code.

Loopback and same-UID process boundaries are not sufficient isolation against
a determined attacker with code execution on your machine.
