# macboot

A native, idempotent macOS developer-machine provisioner — dotfiles, packages, and
system settings from **one declarative binary**. It replaces a pile of Bash scripts (and
GNU Stow) with a single tool that shows you drift, backs up what it touches, and reconciles
your machine to a checked-in config.

> Status: v1 (ongoing management). Fresh-machine bootstrap (Xcode CLT / Rosetta / Homebrew
> install / Oh My Zsh / SDKMAN) is a thin shell shim that ends by calling `macboot apply`.

## Why

- **Replaces stow** with a native symlink engine that records every link it owns, backs up
  displaced files, and can cleanly `unlink`/restore.
- **Idempotent + diffable**: every macOS default is read before it's written, so `diff`
  shows real drift and `apply` only changes what's out of sync.
- **Multi-manager packages**: Homebrew, cargo, npm, pipx, go, and custom providers coexist
  behind one interface; declared-but-missing tools stop with an actionable message.
- **First-class profiles**: personal/work resolved by username/hostname, no `$USER` casing.
- **Honest execution**: every run ends with a `changed / unchanged / failed / skipped`
  summary instead of silently swallowing errors.

## Install

```sh
cargo install --path .          # from a clone
# or: cargo build --release && cp target/release/macboot /usr/local/bin/
```

## Config layout

macboot operates on a **config directory** (see `example/`), resolved via `--config`,
`$MACBOOT_HOME`, a `macboot.toml` in the CWD, or `~/.config/macboot`.

```
macboot.toml       meta, profile match rules, apply stages
packages.toml      brew / cargo / npm / pipx / go / custom providers
macos/*.toml        per-domain defaults, command escape hatch, keybindings
dotfiles/<pkg>/…    stow-style tree, mirrors $HOME
profiles/*.toml     overlays merged onto the base for the active profile
packages/<mgr>/…    output only: `pkg dump` snapshots in each manager's format
```

## Quick start

```sh
macboot init                 # scaffold ~/.config/macboot (or: macboot init ./mydir)
# edit packages.toml / macos/*.toml, drop dotfiles under dotfiles/<pkg>/
macboot status               # preview
macboot apply --dry-run      # then: macboot apply
```

## Commands

```
macboot init [dir] [--force]                            # scaffold a default config
macboot apply [--only …] [--dry-run] [--skip-missing]   # reconcile everything
macboot status                                          # drift summary (read-only)
macboot diff  [--only dotfiles,packages,macos]          # detailed drift

macboot link|unlink|relink [pkg…] [--dry-run]           # stow replacement
macboot adopt --package <pkg> <file>…                   # pull ~/file into a package

macboot pkg apply|diff|clean [--provider …]             # multi-manager packages
macboot pkg dump [--provider …] [--dry-run]             # installed → packages/<mgr>/
macboot pkg list                                        # providers + availability
macboot brew …                                          # alias for pkg --provider brew

macboot macos apply|diff [--only dock,finder,…]         # declarative defaults
macboot macos dump [--domain …] [--output NAME] [--dry-run]  # System Settings → TOML
macboot macos revert [--domain …] [--dry-run]           # undo defaults macboot wrote
macboot keyboard dump [--dry-run]                       # reverse symbolichotkeys → TOML
macboot keyboard apply                                  # import keybindings + reload

macboot profile                                         # active profile + why
macboot doctor            (alias: verify)               # full self-check, non-zero on fail
macboot capture                                         # snapshot machine → manifest form
macboot completions <shell>
```

## Packages

`packages.toml` is the manifest `apply` reads. To go the other way, `pkg dump` snapshots
what is actually installed into `packages/<manager>/`, each file in that manager's own
format — so the output is usable by the manager itself, not just by macboot:

```sh
macboot pkg dump                     # every declared provider
macboot pkg dump --provider brew     # just one
macboot pkg dump --dry-run           # print to stdout, write nothing
```

```
packages/brew/Brewfile            brew bundle --file=packages/brew/Brewfile
packages/npm/package.json         globals as dependencies
packages/mise/mise.toml           [tools], pinned to the installed version
packages/pipx/requirements.txt    one app per line
packages/cargo/crates.txt         xargs cargo install < packages/cargo/crates.txt
packages/<custom>/packages.txt    one package per line
```

Each dump is a full re-render of what is installed, so it replaces the previous snapshot.
These files are **outputs**: `apply` never reads them, and `packages.toml` stays the source
of truth. Providers that can't enumerate what they installed (`go`, `nix`) are skipped.

To move a snapshot back into the manifest, use `macboot capture`, which prints
`packages.toml`-shaped blocks to stdout.

## macOS settings

You don't need to know a `domain`/`key` pair to manage a setting. `macos dump` snapshots
every preference domain, waits while you click around in System Settings, then writes the
difference as `[[defaults]]` blocks into `macos/settings.toml`:

```sh
macboot macos dump                          # all domains (~7s), then tweak and press Enter
macboot macos dump --domain com.apple.dock  # narrower and instant
macboot macos dump --output dock            # write macos/dock.toml instead
macboot macos dump --dry-run                # print to stdout, write nothing
```

`dump` never overwrites an existing `macos/*.toml`; pick another `--output` name, or use
`--dry-run` and merge by hand.

The default mode is a *diff*: it only sees settings you change between the two snapshots, so
it can't capture a machine that's already configured the way you want it. For that, use
`--snapshot` to capture every current value in one pass, with no before/after and no pause:

```sh
macboot macos dump --domain com.apple.dock --snapshot --output dock
```

`--snapshot` still goes through the same noise filter as a diff, so pass `--all` if you want
churn keys included too.

Keys that churn on their own (window frames, recent-item lists, session IDs) are filtered;
pass `--all` to keep them. Values macboot can't yet express as `[[defaults]]` — arrays and
dicts — are emitted as comments rather than silently dropped.

Every key macboot writes has its previous value recorded in the state file first, so changes
are undoable the same way dotfile links are:

```sh
macboot macos revert --dry-run                # show what would be restored
macboot macos revert --domain com.apple.dock  # restore; keys that didn't exist are deleted
```

Revert always returns a key to the value it held *before macboot first touched it*, no matter
how many `apply` runs happened in between.

> Output convention: stdout carries only data (`capture`, and any `dump --dry-run`); all
> progress and summary output goes to stderr, so `macboot macos dump --dry-run > macos/dock.toml`
> produces a clean file.

### What this can't capture

`macos dump`/`apply` only reach settings backed by `defaults` (Preferences plist domains).
Several things people expect a "clone my Mac" tool to carry over live outside that store
entirely and are out of scope here:

- **Privacy permissions** (Full Disk Access, Screen Recording, Camera/Mic, Automation) live
  in the SIP-protected `TCC.db`, not `defaults`. `macboot doctor` checks whether *this
  terminal* has Full Disk Access — without it, `dump`/`apply` can silently skip protected
  domains like Mail, Safari, and Messages — but re-granting permissions to every app on a
  new machine is still a manual step.
- **Keychain items** (Wi-Fi passwords, saved credentials) aren't exported or imported.
- **Login items and LaunchAgents/LaunchDaemons** ("open at login" apps, background helpers)
  aren't captured.
- **Network/Bluetooth/printer state** (Wi-Fi networks, VPN profiles, Bluetooth pairings) live
  in separate stores that need root and different tooling.
- **Apple ID / iCloud sign-in** is deliberately unautomatable by Apple; do it by hand
  regardless of tooling.

Plan on doing these by hand on a new machine; `macboot` covers the `defaults`-backed
preferences, packages, and dotfiles around them.

## Keybindings

macOS stores system hotkeys as an opaque integer-keyed plist. macboot keeps a friendly
form and translates both ways:

```toml
# macos/keyboard.toml
[[hotkey]]
action = "spotlight"      # ↔ symbolichotkeys id 64
enabled = true
chord = "cmd+space"       # ↔ parameters = [65535, 49, 1048576]
```

Never hand-edit the plist — tweak in System Settings, then `macboot keyboard dump` writes a
readable TOML diff. Unknown IDs round-trip as `[[raw]]` entries so nothing is lost.

## Requirements

macboot is a single self-contained binary. Core features (dotfiles, macos defaults,
keybindings) use only stock macOS binaries (`defaults`, `killall`, `pmset`, …). Package
providers require only the CLIs you declare; a missing required tool stops with install
guidance. See the design doc for the fresh-machine bootstrap story.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# Try it safely against the bundled example, in an isolated HOME:
MACBOOT_STATE=/tmp/mb/state.json HOME=/tmp/mb-home \
  cargo run -- --config example link --dry-run
```
