# keygen/

`ralphus-keygen` is the author-only tool that generates the Ed25519 keypair
and signs `ralphus.lic` license files consumed by `auth/`'s `check_license()`.
It is never shipped to users and is not in any release build script — see the
"`keygen` is never shipped" entry in [[../.agent/gotchas|gotchas.md]].

The full workflow, key file formats, and license schema live in
[[../auth/AGENTS|auth/AGENTS.md]] (keygen and auth are documented together
since the signing message format must stay byte-identical between them).
