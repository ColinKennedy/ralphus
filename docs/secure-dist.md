# Secure Distribution

A separate opt-in build of ralphus that refuses to start without a signed
license file. Intended for distributing the software to specific trusted
individuals while keeping the standard open build unchanged.

## How it works

- An Ed25519 keypair is generated once. The private key stays on your machine
  and is never committed or distributed.
- The 32-byte public key is baked into the `auth/` crate at compile time.
- You sign a small JSON license file for each authorized person using your
  private key.
- The recipient drops `ralphus.lic` next to the exe. On startup the daemon and
  librarian verify the signature. If it is missing or invalid they print an
  error and exit.
- Builds without `--features secure-dist` are completely unaffected — the
  check compiles away to nothing.

## New crates (both in their own subfolders, never shipped)

| Crate | Path | Purpose |
|---|---|---|
| `ralphus-auth` | `auth/` | License verification lib used by daemon + librarian |
| `ralphus-keygen` | `keygen/` | Author-only tool: generate keypair, sign licenses |

---

## Step 1 — Generate a keypair (once)

Run from the repo root:

```
cargo run -p ralphus-keygen -- generate
```

Output:

```
Private key -> ralphus-private.key
Public key  -> auth/public.key

IMPORTANT: Keep ralphus-private.key secret. Never commit it.
...
```

`ralphus-private.key` is gitignored. Keep a backup somewhere safe (password
manager, encrypted drive). If you lose it you cannot sign new licenses — you
would need to generate a new keypair and rebuild.

`auth/public.key` is updated in place and should be committed so the next
secure-dist build picks it up.

---

## Step 2 — Rebuild with the public key

After generating (or whenever you want a fresh secure-dist build):

```
cargo build --release \
  --features ralphus-daemon/secure-dist,ralphus-librarian/secure-dist
```

The resulting `target/release/ralphus-daemon.exe` and
`target/release/ralphus-librarian.exe` now require a valid license to start.

---

## Step 3 — Sign a license for someone

```
cargo run -p ralphus-keygen -- sign \
  --key ralphus-private.key \
  --name "Alice" \
  --expiry 2027-07-05
```

Writes `ralphus.lic` in the current directory. Flags:

| Flag | Required | Description |
|---|---|---|
| `--key PATH` | yes | Path to `ralphus-private.key` |
| `--name NAME` | yes | Human-readable name for the holder |
| `--expiry YYYY-MM-DD` | no | If omitted the license never expires |
| `--out PATH` | no | Output path (default: `ralphus.lic`) |

Send the recipient the two secure-dist executables and `ralphus.lic`.

---

## Step 4 — Running as an authorized user

Place `ralphus.lic` in the same directory as `ralphus-daemon.exe` and
`ralphus-librarian.exe`. Both look there automatically.

Alternatively, set `RALPHUS_LICENSE` to the full path of the file:

```
set RALPHUS_LICENSE=C:\keys\ralphus.lic
```

If the license is absent, expired, or tampered with, startup fails:

```
Authorization error: License signature verification failed. This copy is not authorized.
```

---

## Signing for yourself

You still need a license file even for your own machine. The simplest approach:

```
cargo run -p ralphus-keygen -- sign \
  --key ralphus-private.key \
  --name "Colin Kennedy"
```

(No `--expiry` means it never expires.) Drop `ralphus.lic` next to the exes or
set `RALPHUS_LICENSE` to it.

---

## Re-keying

If the private key is ever compromised:

1. Delete `ralphus-private.key`.
2. Run `cargo run -p ralphus-keygen -- generate` to create a new keypair.
3. Commit the new `auth/public.key`.
4. Rebuild with `--features secure-dist`.
5. Re-sign licenses for all current recipients — old `ralphus.lic` files will
   no longer work.
