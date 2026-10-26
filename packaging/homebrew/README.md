# Homebrew Packaging

The release workflow creates platform archives for the Homebrew formula:

- `rayline-<version>-macosx_11_0_arm64.tar.gz`
- `rayline-<version>-macosx_10_12_x86_64.tar.gz`
- `rayline-<version>-linux_x86_64.tar.gz`
- `rayline-<version>-linux_aarch64.tar.gz`

Each archive contains a single `rayline-<version>-<platform>/` directory with
`rayline`, `rld`, `rayline-core`, `rayline-core-source.json`,
`rayline-core-LICENSE`, `README.md`, `LICENSE` and `NOTICE`.

Generate the formula from the release `SHA256SUMS` file and use the matching
archive for each OS/architecture. Do not point Homebrew at bare binary assets;
those are reserved for the install scripts and self-update mirror.

Homebrew installs are intentionally treated as externally managed by
`rayline update`; users should run:

```bash
brew upgrade rayline
```

Set `__RELEASE_BASE_URL__` to
`https://github.com/rayline-ai/rayline/releases/download/v<version>` for public
GitHub releases, or the verified mirror archive directory when rendering a
mirrored formula. Hybrid requires macOS 13+ or Linux; archive platform names
remain stable for existing update clients.
