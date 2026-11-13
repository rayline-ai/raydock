# Releasing and Signing

This document covers the one-time key-generation setup required before shipping
Rayline releases with minisign signature verification.

## Prerequisites

Install [minisign](https://jedisct1.github.io/minisign/):

```sh
brew install minisign      # macOS
apt install minisign       # Debian/Ubuntu
```

---

## 1. Generate the production keypair (one-time)

```sh
minisign -G -p prod.pub -s prod.sec
```

- **Keep `prod.sec` secret.** Never commit it. Store it in a password manager
  or hardware key that CI can access only at release time.
- `prod.pub` is the public key. You will embed it in the source code (see step 3).

---

## 2. Store the secret key as a GitHub Actions secret

1. Open **Settings → Environments** in your GitHub repository.
2. Create an environment named **`release`**.
3. Add **Required reviewers** (at least one human) so signing cannot happen
   unattended.
4. Add the secret `MINISIGN_SECRET_KEY` and paste the full contents of
   `prod.sec` as its value.

The CI workflow (`release.yml`) reads this secret from the `release` environment
in the `sign` job. The `publish` job depends on `sign`, so no release asset
reaches GitHub without a valid signature.

---

## 3. Embed the public key in source

The public key is pinned in three places, which must match. Set each to the
base64 key from `prod.pub` (the single line after `untrusted comment:`):

- `crates/rayline-cli/src/lib.rs`, read by `rayline update`:

  ```rust
  pub const MINISIGN_PUBLIC_KEYS: &[&str] = &[
      "<prod-pubkey-base64>", // rayline production signing key
  ];
  ```

- `scripts/install-rayline.sh`:

  ```sh
  RAYLINE_PUBKEY="<prod-pubkey-base64>"
  ```

- `scripts/install-rayline.ps1`:

  ```powershell
  $PublicKey = "<prod-pubkey-base64>"
  ```

The installers trust only this pinned key. No env var overrides it.

Commit the changes and ship. The new key takes effect for all subsequent
`rayline update` calls and fresh installs.

---

## 4. Key rotation plan

When you need to rotate (key compromise, periodic rotation, hardware change):

1. Generate a new keypair: `minisign -G -p prod2.pub -s prod2.sec`
2. Add `prod2.pub`'s key string to `MINISIGN_PUBLIC_KEYS` **alongside** the old
   key (do not remove the old key yet):
   ```rust
   pub const MINISIGN_PUBLIC_KEYS: &[&str] = &[
       "RW... (old key)",
       "RW... (new key)",
   ];
   ```
3. Ship a release signed with the new key. Users who auto-update will accept
   signatures from either key.
4. After at least **2 releases** (so that auto-updaters have moved past the
   transition), remove the old key from the array and ship a final release.
5. Revoke the old secret from GitHub Secrets and destroy the old `prod.sec`.

---

## 5. Verifying a release manually

```sh
minisign -Vm SHA256SUMS -P <prod-pubkey-base64>
```

If the signature is valid, minisign prints:
```
Signature and comment signature verified
```

---

## Security notes

- The `sign` CI job is gated on the `release` GitHub environment (required
  reviewers). No automated process can sign without a human approving the run.
- `verify_signature` in `update.rs` is fail-closed: a missing or invalid
  `.minisig` causes `rayline update` to abort before any binary is installed.
- Both installers require minisign and abort if the signature doesn't verify.
- Private keys must never be committed to the repository. Only public keys
  (or their base64 strings) belong in source.
