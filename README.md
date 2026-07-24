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

macboot pkg apply|diff|clean|dump [--provider …]        # multi-manager packages
macboot pkg list                                        # providers + availability
macboot brew …                                          # alias for pkg --provider brew

macboot macos apply|diff [--only dock,finder,…]         # declarative defaults
macboot keyboard dump [--stdout] [--dry-run]            # reverse symbolichotkeys → TOML
macboot keyboard apply                                  # import keybindings + reload

macboot profile                                         # active profile + why
macboot doctor            (alias: verify)               # full self-check, non-zero on fail
macboot capture                                         # snapshot machine → manifest form
macboot completions <shell>
```

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
