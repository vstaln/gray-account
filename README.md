<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
</p>
<h1 align="center">gray-account</h1>
<p align="center">Sign in to the gray registry and scaffold, check, build and publish gray plugins.</p>
<p align="center">
  <a href="https://github.com/vstaln/gray-account/blob/main/LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-7aa2f7.svg">
  <img alt="rust" src="https://img.shields.io/badge/built%20with-rust-orange.svg">
</p>

Account login and plugin making for [gray](https://github.com/vstaln/gray):
exchanges an enrollment code from
[gray.alignment.id](https://gray.alignment.id/account) for a registry token,
answers `whoami` and `logout`, and scaffolds/checks/builds/releases/publishes
gray plugins. The standalone `gray-maker` plugin was merged into this one —
every `gray-maker <cmd>` is now `gray account <cmd>` (`ship` is gone;
`publish` runs the whole pipeline).

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

In the REPL, `/maker` is the plugin-making command: `/maker new weather`
scaffolds inline, and `/maker <check|build|release|publish>` returns a
prompt asking the agent to run the step in a shell (each takes minutes).

In a shell:

```sh
gray account login [code]
gray account whoami
gray account logout

gray account new <name> [--dir D] [--description TEXT] [--no-repo]
gray account check
gray account build [--remote HOST]
gray account release
gray account publish [--remote HOST] [--dir PATH]
```

`check`, `build`, `release` and `publish` act on the plugin in the current
directory (or `--dir`); `new` scaffolds `~/grayplugins/gray-<name>` and its
GitHub repo. `publish` is the whole pipeline: registry preflight → check →
adopt an existing release or build+release → submit → confirm.

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

---
Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
