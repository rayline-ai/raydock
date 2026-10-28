# Security Policy

## Supported Versions

Raydock, the local-router project in the broader Rayline ecosystem, is
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

## Local TLS Interception

Raydock can install a local certificate authority and intercept TLS
traffic on your own machine to route requests. This is core to how proxy-mode
routing works and is documented in the [README](README.md#disclaimers).

Reports about this intended behavior are not security vulnerabilities. Reports
about flaws in this mechanism, such as certificate handling, key storage, or
privilege escalation, should be reported through the channels above.

## Trust Model

This section documents what Rayline's local security mechanisms protect against,
what they do not protect against, and what residual risks remain.

### Threat boundary

**Loopback is not a security boundary on a single-user machine.** Any process
running as the same OS user can connect to the local proxy port, read the
router API key from the process environment, or read key material from the
filesystem (subject to file permissions). Rayline's local protections are
defense-in-depth measures — they are not a hard isolation boundary.

For strong isolation (e.g., when running untrusted local code alongside
Rayline), use OS-level isolation: a separate OS user account, a container, or
a virtual machine.

### Router API key

The router API key (`RAYLINE_ROUTER_API_KEY`) authenticates Claude Code
sessions to the Rayline router. It is:

- **Revocable server-side**: keys are minted via `POST /v1/keys` and revoked
  via `POST /v1/auth/cli/revoke` (see `crates/rayline-cli/src/status.rs`,
  functions `mint_router_key_at` and `revoke_rayline_session`).
- **Stripped from Claude Code's child environment**: the launcher calls
  `command.env_remove("RAYLINE_ROUTER_API_KEY")` before spawning Claude Code
  (`crates/rayline-cli/src/claude.rs`, `configure_proxy_auth_env`), so the key
  is not inherited by the Claude Code process or its children.
- **Residual risk**: the key remains readable in the `rld` daemon's process
  environment via `/proc/<pid>/environ` on Linux (or equivalently via
  `ps eww <pid>` / `proc_pidinfo` on macOS) by any process running as the
  same UID. This is a known residual. Same-UID isolation requires OS-level
  controls.

### Local CA

The local certificate authority used for TLS interception is:

- **Scoped to the launched process via `NODE_EXTRA_CA_CERTS`**: the CA cert is
  passed to Claude Code's child process through this environment variable
  (`crates/rayline-cli/src/claude.rs`, `node_ca_bundle_value` and
  `configure_proxy_env`). It is not installed into the OS trust store or any
  browser trust store.
- **Stored in a 0700 directory**: the CA directory is created with mode `0700`
  on Unix so that other users on the same machine cannot read the private key
  (`crates/rayline-proxy/src/lib.rs`, `LocalCa::load_or_generate`).
- **CA private key stored 0600**: the key file is written with `OpenOptions`
  mode `0o600` (`crates/rayline-proxy/src/lib.rs`, `write_private_key`).
- **Bounded 180-day lifetime**: generated CAs expire 180 days after creation
  (backdated 5 minutes for clock skew). The load path checks expiry and
  regenerates automatically within 7 days of expiry, so TLS interception does
  not silently break after the CA ages out.
- **Not in the OS trust store**: Rayline never calls `security add-trusted-cert`
  (macOS), `certutil` (Linux/Windows), or any equivalent. The CA only affects
  the single Claude Code child process it is injected into.

### Recommended isolation for untrusted workloads

If you run untrusted local code alongside Rayline (e.g., MCP servers from
unknown sources, arbitrary shell commands), use OS-level isolation:

- Run Rayline under a dedicated OS user account, or
- Use a container or VM that separates Rayline from untrusted code.

Loopback and same-UID process boundaries are not sufficient isolation against
a determined attacker with code execution on your machine.
