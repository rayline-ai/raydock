# Homebrew Packaging

Production Homebrew publishing is owned by `atlasfutures/memex-desktop`:
its `backend/workshop-cli/homebrew/raydock.rb.template` installs the flat
`homebrew/` archives produced by that repo's release workflow.

This directory's template is for the manual fallback workflow in this repo,
whose archives have the layout below. Do not render it against production's
flat archives or substitute one archive layout for the other.

The fallback workflow creates platform archives:

- `rayline-<version>-macosx_11_0_arm64.tar.gz`
- `rayline-<version>-macosx_10_12_x86_64.tar.gz`
- `rayline-<version>-linux_x86_64.tar.gz`
- `rayline-<version>-linux_aarch64.tar.gz`

Each archive contains a single `rayline-<version>-<platform>/` directory with
`raydock`, its `rayline` alias, `rld`, `rayline-core`, `rayline-core-source.json`,
`rayline-core-LICENSE`, `README.md`, `LICENSE` and `NOTICE`.

Generate the formula from the release `SHA256SUMS` file and use the matching
archive for each OS/architecture. Do not point Homebrew at bare binary assets;
those are reserved for the install scripts and self-update mirror.

Homebrew installs are intentionally treated as externally managed by
`raydock update`; users should run:

```bash
brew upgrade raydock
```

Set `__RELEASE_BASE_URL__` to
`https://github.com/rayline-ai/raydock/releases/download/v<version>` for public
GitHub releases, or the verified mirror archive directory when rendering a
mirrored formula. Hybrid requires macOS 13+ or Linux; archive platform names
remain stable for existing update clients.
