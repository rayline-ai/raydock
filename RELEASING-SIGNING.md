# Releasing and Signing

This document covers the one-time key-generation setup required before shipping
Rayline releases with minisign signature verification.

## Status (2026-06-23) — production key provisioned

- ✅ Production keypair generated (minisign, passphrase-less for unattended CI).
- ✅ Public key embedded in `crates/rayline-cli/src/lib.rs` (`MINISIGN_PUBLIC_KEYS`) and
  `scripts/install-rayline.sh` (`RAYLINE_PUBKEY`): `RWRKGvuHHJS76PGzxmnM/1NX8SFhTi3mPj/axsIjv/Ehnw71G4Ei9xb1`.
- ✅ `MINISIGN_SECRET_KEY` set as a secret on the protected `release` GitHub environment
  (required reviewer: `chilang`; self-review allowed for the solo maintainer).
- ✅ Keypair verified end-to-end (sign with secret → verify with the committed public key).
- ⏳ **REMAINING (human):** back up the secret key to 1Password — a `0600` copy was left at
  `~/rayline-prod-minisign.key`; stash it, then shred the file. Without a backup, the only copy
  is the (unreadable) GitHub secret, and recovery means rotating to a new key.
- ⏳ **REMAINING:** verify the first signed release end-to-end (a real tag → CI signs → `raydock update` accepts).

The sections below are the original setup guide / rotation reference.

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

## 3. Replace the placeholder public key in source

Open `crates/rayline-cli/src/lib.rs` and find:

```rust
pub const MINISIGN_PUBLIC_KEYS: &[&str] = &[
    "RWRqzAWsbJCJh9W2BSnYcbRiBwshTgouNtwYqkmFX1Qs6kXdxY70sRCP", // test placeholder — REPLACE BEFORE SHIPPING
];
```

Replace the placeholder with the base64 key from `prod.pub` (the single line
after `untrusted comment:`).

Also update the same placeholder in `scripts/install-rayline.sh`:

```sh
RAYLINE_PUBKEY="${RAYLINE_MINISIGN_PUBKEY:-<your-prod-key>}"
```

Commit both changes and ship. The new key takes effect for all subsequent
`raydock update` calls and fresh installs.

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
  `.minisig` causes `raydock update` to abort before any binary is installed.
- The installer (`install-rayline.sh`) follows the same pattern: when
  `minisign` is present, it verifies and aborts on failure; when absent, it
  prints a TOFU notice and proceeds over HTTPS.
- Private keys must never be committed to the repository. Only public keys
  (or their base64 strings) belong in source.
