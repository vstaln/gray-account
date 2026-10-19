# WHY.md — account plugin changes (in flight)

## Why this tree looks like this

Sibling of the `~/gray` core→plugin extraction (see `~/gray/WHY.md`):
`gray login`/`whoami`/`logout`/`cloud` moved out of core into this plugin,
which now owns every account surface.

Uncommitted on top of the released 0.5.0:

- `main.rs` — `login -h|--help` prints usage instead of passing `--help`
  to `account::login` as an enrollment code (registry round-trip + error
  for what is obviously a help request). Reviewer-flagged; matches how
  every other verb in this CLI self-documents.

Verified: `gray-account login --help` prints usage; bare `login` still
runs the enrollment flow. `cargo check`/`clippy -D warnings`/`fmt` clean.

## maker namespace verb

Core's claim contract is `commands: ["/x"]` -> `plugin-bin x args` (verb
prepended). `/maker` was a *namespace* claim that relied on the old
no-prepend convention (`/maker new x` -> `gray-account new x`). Under the
new contract that becomes `gray-account maker new x`, so a `maker` verb
was added that re-dispatches argv[1..] into maker::dispatch. Verified:
`gray-account maker` -> usage, `maker bogus` -> unknown-command error,
`maker new x` reaches dispatch. Release-ordering: ship this with the
`login --help` fix before/with the core extraction merge.
