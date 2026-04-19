# Contributing to Ferrite

Thanks for your interest. This guide is the short version — the long
version lives in `docs/`, which you should skim at least once (the file
list is in the [README](README.md#documentation)).

## Before you start

- Read [`docs/00-context.md`](docs/00-context.md) and
  [`docs/01-architecture.md`](docs/01-architecture.md) so you know what
  Ferrite is trying to be.
- Follow [`docs/06-build.md`](docs/06-build.md) to get a local dev
  environment up. It covers prereqs (SoapySDR, Rust, pnpm), udev rules,
  SDRPlay, and the daily dev loop.
- Look at [`docs/10-commits.md`](docs/10-commits.md) to see where the
  project currently is in its phase plan. Pick work that's in-phase; if
  you want to jump ahead, raise it in an issue first.

## Commit style

The project follows [Conventional Commits](https://www.conventionalcommits.org/)
with a small set of types: `feat`, `fix`, `chore`, `docs`, `test`,
`refactor`, `build`, `ci`. Scope is optional but encouraged —
`feat(server): …`, `chore(blocks): …`.

Two rules carry most of the weight (see [D16](docs/09-decisions.md#d16--conventional-commits-green-at-every-commit)):

- **Every commit green.** `cargo check/test/clippy` and
  `pnpm -r lint/check/test` all pass at every commit, not just at the
  tip of a branch. Bisect depends on this.
- **One conceptual change per commit.** If a diff can be split into
  "add a thing" + "use the thing," make it two commits. PRs may contain
  multiple commits.

Tests land with the code they test. No committed `TODO` comments —
either do it, file an issue, or delete it.

## Pre-commit hooks

Installed automatically by `pnpm install` (the root `prepare` script
runs `lefthook install`). On commit, the hook runs in ~2.5 seconds:

| check       | tool                           | gated on           |
|-------------|--------------------------------|--------------------|
| formatting  | `cargo fmt --check`            | `*.rs` staged      |
| lint/format | `pnpm -r lint`                 | TS/JSON/MD/etc.    |
| typecheck   | `pnpm -r check`                | `*.{ts,svelte}`    |

Clippy and the workspace test suites are left to CI — too slow for a
pre-commit budget. Configuration lives in [`lefthook.yml`](lefthook.yml).

If the hook blocks a commit, fix the issue and re-commit. Never use
`--no-verify` to bypass it; that turns a five-second feedback loop into
a seven-minute CI round trip for everyone downstream.

## CI gates

Two workflows run on every push to `main` and every PR:

- **`rust`** (`.github/workflows/rust.yml`) — `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace --all-targets`, plus a wasm32 build to
  catch dual-compile regressions in `ferrite-blocks`.
- **`pnpm`** (`.github/workflows/web.yml`) — `pnpm install
  --frozen-lockfile`, then `pnpm -r lint/check/test` across every
  workspace package.

If both are green locally (and the pre-commit hook passed), CI is
almost always green too. Playwright E2E and the SAB stress test land
in Phase B/D workflows as those features arrive.

## Pull requests

- **Small, focused.** Big PRs stall. If your branch has grown past
  ~400 lines of diff, look for a clean split point.
- **Green at every commit**, not just the last one. If you need to
  rebase to fix mid-branch breakage, do so before pushing for review.
- **Describe the why, not the what.** The diff shows what; the
  description should say why it's worth landing.
- **Link the commit-plan item** it addresses if there is one
  (`closes #36`, or `advances docs/10-commits.md item 36`).

Reviewers look for: does this match the design in `docs/`? Is the test
coverage honest? Are there committed `TODO`s or placeholder
implementations? Are new files in the right workspace?

## Adding a new block

A DSP block is a folder in `packages/flowgraph-blocks/src/blocks/`
with a fixed contract (`spec.json`, `block.ts`, `index.ts`, `README.md`).
[`src/blocks/decimator/`](packages/flowgraph-blocks/src/blocks/decimator/)
is a working example. Rust-backed blocks additionally live
under `blocks/src/`; the `#[ferrite_block]` proc macro (commit #33)
will generate `spec.json` from the Rust side once it lands.

See [`docs/03-blocks.md`](docs/03-blocks.md) for the full contract and
[`docs/04-flowgraphs.md`](docs/04-flowgraphs.md) for how blocks compose
into a flowgraph.

## Adding a test IQ sample

Test captures live under `samples/` with a filename convention
(`<mode>_<freq>mhz_iq-<fmt>.wav`) and a per-file JSON sidecar. See
[`samples/README.md`](samples/README.md) for the rules — attribution
and licence metadata are mandatory.

## Where to ask questions

Open a GitHub Discussion for design questions, an issue for concrete
bugs or feature requests. Don't open a PR for anything non-trivial
without prior discussion — it's faster for everyone to align on the
approach before the code is written.
