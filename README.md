# gray-account

Account login for [gray](https://github.com/vstaln/gray): exchanges an
enrollment code from [gray.alignment.id](https://gray.alignment.id/account)
for a registry token, then answers `whoami` and `logout`.

## Install

```sh
cargo install --git https://github.com/vstaln/gray-account --locked
gray plugin install account
```

`gray plugin install account` finds `gray-account` on `PATH` and registers
it in `~/.gray/plugins/lock.json`.

## Usage

In the gray REPL:

```
/login <code>
/whoami
/logout
```

In a shell:

```sh
gray account login [code]
gray account whoami
gray account logout
```

Mint a login code at <https://gray.alignment.id/account> — sign in with
GitHub, Google, or Discord and choose "Generate CLI login code" (it expires
in 5 minutes).

## The token

Exchanged tokens are written to `~/.gray/registry-token.json`, mode `0600`,
via an atomic rename — the same file older gray builds wrote, so an existing
login keeps working after installing the plugin.

`GRAY_REGISTRY_URL` overrides the registry base URL (default
`https://gray.alignment.id/api`) for pointing at a local backend.

Honest note: nothing in gray currently gates functionality on an account —
the token only names you on registry calls.
