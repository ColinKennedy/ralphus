# auth/ (and keygen/)

## Cryptography / Secure Distribution

See [`docs/secure-dist.md`](../docs/secure-dist.md) for the full workflow. Summary:

**What it is:** An opt-in build mode (`--features secure-dist`) where `ralphus-daemon` and `ralphus-librarian` refuse to start without a signed `ralphus.lic` file. Standard open builds are completely unaffected — the check compiles away to nothing without the feature flag.

**Crates:**
- `auth/` — `ralphus-auth` lib; exports `check_license()`. Uses **ed25519-dalek** for Ed25519 signature verification, **base64** for signature encoding. Public key is embedded at compile time via `include_bytes!("../public.key")`.
- `keygen/` — `ralphus-keygen` bin (author-only, never distributed — see [[../.agent/gotchas|gotchas.md]]). Uses **rand_core::OsRng** for entropy. Subcommands: `generate` (keypair) and `sign` (license file).

**License file format** (`ralphus.lic`, JSON):
```json
{ "holder": "Alice", "seat": "alice@BOX", "expiry": "2027-01-01", "signature": "<base64-ed25519>" }
```
Message signed: `"RALPHUS|<holder>|<seat>|<expiry>"`, where an absent `seat` is the literal `any` and an absent `expiry` is the literal `never`. **`keygen/src/main.rs` builds this string independently of `auth::signing_message` — the two must stay byte-identical, and each has a test pinning the format.** Expiry is compared as a string (`YYYY-MM-DD` lexicographic order). `seat` is a `user@hostname` binding (see the **seat** entry in `docs/glossary.md` — deliberately not called a *machine*, which RAL-185 already took); it is compared case-insensitively against the local `USERNAME`/`USER`/`LOGNAME` plus the `gethostname` crate's hostname. Order of checks is signature → expiry → seat, so a forged file always reports "not authorized" rather than leaking which field was wrong.

**Key files:**
- `auth/public.key` — 32-byte raw Ed25519 public key; baked into the binary at compile time; **committed**.
- `ralphus-private.key` — hex-encoded seed (64 chars); **gitignored**; never distributed; back it up.

**Full workflow:**
```bash
# 1. Generate a keypair (once; overwrites auth/public.key)
cargo run -p ralphus-keygen -- generate

# 2. Rebuild with the new public key baked in
cargo build --release --features ralphus-daemon/secure-dist,ralphus-librarian/secure-dist

# 3. Sign a license for someone
cargo run -p ralphus-keygen -- sign \
  --key ralphus-private.key \
  --name "Alice" \
  --seat alice@HER-BOX \  # or --this-seat; omit both to run anywhere
  --expiry 2027-01-01     # omit for non-expiring license

# 4. Recipient drops ralphus.lic next to the executables
#    (or sets RALPHUS_LICENSE=<path>)
```

Re-keying: delete `ralphus-private.key`, run `generate` again, commit the new `auth/public.key`, rebuild, re-sign all existing recipients — old license files will no longer verify.
