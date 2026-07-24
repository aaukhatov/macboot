# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```sh
cargo test                                   # all tests (unit tests live in-module under #[cfg(test)])
cargo test dotfiles::tests::plan_creates_link_for_new_target   # a single test
cargo test -- --nocapture                    # see println!/ui output
cargo clippy --all-targets -- -D warnings    # CI gate
cargo fmt --all --check                      # CI gate
cargo build --release
```

CI (`.github/workflows/ci.yml`) runs fmt → clippy → test → release build on `macos-latest`.

Run the binary safely against the bundled `example/` config without touching the real machine —
override both `$HOME` and the state file, and prefer `--dry-run`:

```sh
MACBOOT_STATE=/tmp/mb/state.json HOME=/tmp/mb-home \
  cargo run -- --config example link --dry-run
```

Anything that shells out to `defaults`, `brew`, or `killall` will hit the real machine even under a
fake `$HOME`, so use `--dry-run` for `macos`/`pkg` commands during development.

## Architecture

A single binary (`src/main.rs`) that reconciles a macOS machine to a declarative config directory.
Three layers, strictly ordered:

1. **CLI** (`main.rs`) — clap `derive` enums only; every arm delegates to `commands::`. `completions`
   and `init` are handled before config loading because they must work with no config present.
2. **Commands** (`commands/mod.rs`) — thin glue. Builds a `Ctx` (config dir, profile override,
   `--yes`), loads `Config` + `State`, calls one feature module, then `summary.render()`. No domain
   logic belongs here.
3. **Feature modules** — `dotfiles/`, `pkg/`, `macos/`. Each takes `&Config` (+ `&mut State` where it
   mutates the filesystem) and returns a `ui::Summary`.

### Config resolution (`config/mod.rs`)

`Config::load` produces one fully-resolved struct per run: `macboot.toml` (meta, profile match rules,
apply stages) + `packages.toml` + every `macos/*.toml` in filename order + the dotfiles package list,
with `profiles/<active>.toml` merged on top. Overlays are **additive only** — `Packages::merge`
unions lists, never replaces (`config/packages.rs`). The config root is canonicalized so recorded
symlinks are absolute and stable.

Config directory precedence: `--config` → `$MACBOOT_HOME` → a `macboot.toml` in the CWD →
`$XDG_CONFIG_HOME/macboot` → `~/.config/macboot`. Note the deliberate use of `~/.config` rather than
`dirs::config_dir()`, which resolves to `~/Library/Application Support` on macOS.

Profile resolution (`profile.rs`): `--profile` → first matching `[[profile.match]]` rule by
username/hostname → `[profile].default` → `"personal"`. A rule with no predicates never matches.

### State and symlink ownership (`state.rs`, `dotfiles/`)

The stow replacement's core invariant: **macboot only removes links it recorded**. Every created
symlink becomes a `LinkRecord` in the machine-local state file (`$MACBOOT_STATE`, else
`~/.local/state/macboot/state.json` — outside the config repo), including where a displaced file was
backed up. `unlink` walks records, verifies the link still points at our source, removes it, and
restores the backup. Backups live in `<state dir>/backups/<timestamp>/`.

`dotfiles::plan` walks each package tree (mirroring `$HOME`) and classifies each file leaf into an
`Action`: `Create` / `AlreadyLinked` / `Repoint` (our link, wrong target) / `Backup` (foreign file —
move aside first) / `Blocked`. `link` executes the plan; `status` reports it read-only. Both go
through the same `plan`, so drift reporting and application can never disagree.

`Blocked` guards a sharp edge: `symlink_metadata` follows symlinked *parent* directories, so a
target under a linked dir looks like an ordinary foreign file and would be "backed up" — moving
the very repo file the link points at. `symlinked_ancestor` detects this and refuses. For the same
reason `adopt` never creates a directory symlink: a directory is adopted as its individual file
leaves.

### Packages (`pkg/`)

Every backend implements the `Provider` trait; `diff`/`apply`/`clean`/`dump` in `pkg/mod.rs` are
provider-agnostic. Two implementations exist:

- `pkg/brew.rs` — bespoke: renders a temp Brewfile and runs `brew bundle` (idempotent, covers
  taps/formulae/casks/mas/vscode in one shot). Drift scope is `brew leaves` + casks only.
- `pkg/generic.rs::CmdProvider` — everything else, built purely from command templates with `{pkg}`
  substitution. Covers cargo/npm/pipx/go/mise/macports/nix and user `[providers.custom.*]` blocks.
  Providers that cannot enumerate installed packages set `ListParse::None` and become apply-only
  (`supports_diff() == false`).

To add a built-in flat-list provider: add the field to `Packages`, add it to
`Packages::list_providers`, add a match arm in `generic::builtin` (including its `dump_file` /
`DumpFormat`), and add its manifest key to `generic::items_key` (used by `capture`). No change to
the command layer.

Two different dumps exist, and they are not interchangeable:

- `pkg::dump` (`pkg dump`) writes `packages/<provider>/<file>` in the *manager's* native format —
  `Provider::dump_file` + `dump_native` (Brewfile, package.json, mise.toml, else a bare
  newline-delimited list). Output only: nothing reads these back, `packages.toml` stays the
  manifest. Keep list files comment-free — they are meant for `xargs`.
- `pkg::dump_toml` (`capture`) prints `packages.toml` blocks to stdout via `Provider::dump_block`,
  which is the one that must round-trip into the manifest.

Providers with `supports_diff() == false` can't enumerate what they installed, so `pkg dump` skips
them rather than writing an empty file.

`registry()` only ever builds providers that are *declared* in the manifest — an undeclared manager
is never touched. `preflight()` fails on a missing **required** CLI (unless `--skip-missing`) and
prints remediation from `Provider::remediation`.

### macOS settings (`macos/`)

Every `defaults` value is read via `is_in_sync` before it is written, which is what makes `apply`
idempotent and `diff` meaningful. A `macos/*.toml` file may contain `[[defaults]]`, `[[command]]`
(escape hatch, optional `sudo`), `[[app_shortcut]]`, `[[hotkey]]`/`[[raw]]`, and `killall`.
`killall` targets are deduplicated across all files and run once at the end of `apply` — but only
for files that actually changed something, since restarting Dock/Finder is user-visible.

`--only` names are validated against the files that exist (`selected_files` returns `Result`): a
mistyped filter must never look like a successful no-op.

`macos::apply` takes `&mut State`. Before the *first* write to any key it records that key's
previous value and `defaults read-type` into `state.defaults`, which is what `macos revert`
replays — a key that did not exist is deleted, one that did is written back with its original
type. Later applies never overwrite the recorded original, so revert always reaches the
pre-macboot state.

`macos/dump.rs` is the discovery half (`macos dump`): it exports every domain (8 scoped threads,
~3s for 736 domains), waits on `ui::pause`, exports again, and renders the diff as `[[defaults]]`
blocks. Churn keys are filtered via `NOISY_KEY_FRAGMENTS`/`NOISY_DOMAINS`; shapes that
`[[defaults]]` cannot express become comments so the output always parses back into a `MacosFile`.

Both dumps (`macos dump`, `keyboard dump`) write through `commands::write_macos_file`: a real run
writes `macos/<name>.toml`, `--dry-run` prints the TOML to stdout instead. `keyboard dump` owns
`keyboard.toml` (a full re-render) and overwrites it; `macos dump` emits a diff and so refuses to
clobber an existing file.

`macos/keyboard.rs` translates both directions between the opaque `com.apple.symbolichotkeys` plist
(`parameters = [ascii, keycode, modifierMask]`) and friendly `action`/`chord` TOML. `ACTIONS` and
`KEYCODES` are the lookup tables; unknown IDs round-trip as `[[raw]]` entries so a dump never loses
data. `apply` writes a temp plist, `defaults import`s it, then runs `activateSettings -u`.

## Conventions

- **All process execution goes through `proc.rs`.** `capture` never fails on non-zero exit (caller
  inspects `status`), `run` streams stdio and errors on non-zero, `output` returns stdout or an error
  carrying stderr. Do not use `std::process::Command` directly elsewhere.
- **`--dry-run` must be honored in every mutation path**, and dry runs still record `Status::Changed`
  in the summary (labelled `[dry-run]`) so the preview matches the real run.
- **All user-visible output goes through `ui.rs`** (`info`/`warn`/`err`/`ok`/`detail`/`heading`).
  `Summary::record` both counts an outcome *and* prints its line — don't print the line separately.
  Failures are collected and re-printed by `render()`.
- **Every `dump` writes a file; `--dry-run` prints it to stdout instead.** They all go through
  `util::write_generated`, so the behavior can't drift apart. Default destinations:
  `macos/<--output>.toml` (default `settings`, refuses to clobber), `macos/keyboard.toml`
  (replaced), `packages/<provider>/<native file>` (replaced).
- **`ui.rs` writes to stderr; stdout is data only.** The TOML from `capture` and the body of any
  `dump --dry-run` is the only thing on stdout, so `… --dry-run > macos/dock.toml` works. Never
  `println!` commentary.
- **Anything fed back into a manifest must round-trip.** It has to parse into the same struct that
  produced it — hence `Provider::dump_block` (brew overrides it to emit `taps`/`brews`/`casks`
  rather than a key `BrewSpec` would ignore) and `#[serde(deny_unknown_fields)]` on every spec in
  `config/packages.rs`. The native `pkg dump` files round-trip into *their own manager* instead.
- **Errors use `anyhow` with `.context()`** naming the path or command involved; `main` prints
  `{e:#}` and exits 1.
- Exit codes are load-bearing: `pkg diff` exits 1 on drift and `doctor` exits 1 on failure so they
  can gate CI/pre-commit.
- Failure policy is per-stage, driven by `[apply].on_error` (`continue` default, or `abort`); a
  failed stage still records into the summary.
- Rust lines wrap at 100 chars (`.editorconfig`); everything else at 120.

## Testing notes

Tests are unit tests inside each module. Tests that mutate process-global env (`HOME`, `USER`) —
`dotfiles/mod.rs`, `profile.rs` — must serialize on the module's `ENV_LOCK` mutex; cargo runs tests
in threads and overlapping mutation causes flakes. `dotfiles` tests build a real config tree in a
`tempfile::tempdir()` and point `HOME` at it.
