//! macboot — a native, idempotent macOS developer-machine provisioner.
//! See README.md and the design doc for the full model.

mod commands;
mod config;
mod dotfiles;
mod macos;
mod pkg;
mod proc;
mod profile;
mod state;
mod ui;
mod util;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use commands::Ctx;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "macboot",
    version,
    about = "Manage a macOS dev machine: dotfiles, packages, and settings.",
    propagate_version = true
)]
struct Cli {
    /// Path to the config directory (overrides $MACBOOT_HOME and defaults).
    #[arg(long, global = true, value_name = "DIR")]
    config: Option<PathBuf>,

    /// Force a machine profile instead of resolving from match rules.
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,

    /// Assume "yes" for confirmation prompts.
    #[arg(long, short = 'y', global = true)]
    yes: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Reconcile the whole machine to the manifest (packages, dotfiles, macos).
    Apply {
        /// Restrict to these stages (comma-separated: packages,dotfiles,macos).
        #[arg(long)]
        only: Option<String>,
        #[arg(long)]
        dry_run: bool,
        /// Skip providers whose CLI is missing instead of failing.
        #[arg(long)]
        skip_missing: bool,
    },
    /// Read-only drift summary across all domains.
    Status,
    /// Detailed intended-vs-current diff (optionally filtered).
    Diff {
        #[arg(long)]
        only: Option<String>,
    },

    /// Create dotfile symlinks (stow replacement).
    Link {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove macboot-owned dotfile links; restore backups.
    Unlink {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Unlink then link (after moving files within a package).
    Relink {
        packages: Vec<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Move an existing ~/file into a package and link it back.
    Adopt {
        #[arg(long)]
        package: String,
        files: Vec<PathBuf>,
        #[arg(long)]
        dry_run: bool,
    },

    /// Package operations across providers.
    Pkg {
        #[command(subcommand)]
        cmd: PkgCmd,
    },
    /// Convenience alias for `pkg --provider brew`.
    Brew {
        #[command(subcommand)]
        cmd: PkgCmd,
    },

    /// macOS settings operations.
    Macos {
        #[command(subcommand)]
        cmd: MacosCmd,
    },
    /// System keybindings (com.apple.symbolichotkeys).
    Keyboard {
        #[command(subcommand)]
        cmd: KeyboardCmd,
    },

    /// Scaffold a default config directory to get started fast.
    Init {
        /// Where to create the config (default: --config, $MACBOOT_HOME, or ~/.config/macboot).
        dir: Option<PathBuf>,
        /// Overwrite the top-level macboot.toml if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Show the active profile and why it matched.
    Profile,
    /// Full self-check (alias: verify); non-zero exit on failure.
    #[command(alias = "verify")]
    Doctor {
        #[arg(long)]
        fix: bool,
    },
    /// Snapshot the current machine into manifest form (stdout).
    Capture,
    /// Generate shell completions.
    Completions { shell: Shell },
}

#[derive(Subcommand)]
enum PkgCmd {
    /// Install declared packages (idempotent).
    Apply {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        skip_missing: bool,
    },
    /// Show per-provider drift; non-zero exit if any drift.
    Diff {
        #[arg(long)]
        provider: Option<String>,
    },
    /// Remove extraneous packages (confirmation-gated).
    Clean {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Snapshot installed packages into packages/<provider>/ in each manager's
    /// own format (Brewfile, package.json, ...).
    Dump {
        #[arg(long)]
        provider: Option<String>,
        /// Print to stdout instead of writing the files.
        #[arg(long)]
        dry_run: bool,
    },
    /// List declared providers and availability.
    List,
}

#[derive(Subcommand)]
enum MacosCmd {
    Apply {
        #[arg(long)]
        only: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    Diff {
        #[arg(long)]
        only: Option<String>,
    },
    /// Snapshot, wait while you change System Settings, then write the diff as
    /// `[[defaults]]` TOML to macos/<NAME>.toml.
    #[command(alias = "record")]
    Dump {
        /// Limit the snapshot to these domains (repeatable, much faster).
        #[arg(long, value_name = "DOMAIN")]
        domain: Vec<String>,
        /// File to write, as macos/<NAME>.toml (default: settings).
        #[arg(long, value_name = "NAME")]
        output: Option<String>,
        /// Include keys macboot filters as churn (window frames, recents, ...).
        #[arg(long)]
        all: bool,
        /// Print to stdout instead of writing the file.
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore defaults macboot has changed to their pre-macboot values.
    Revert {
        /// Only revert this domain.
        #[arg(long, value_name = "DOMAIN")]
        domain: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum KeyboardCmd {
    /// Reverse the live symbolichotkeys domain into macos/keyboard.toml.
    Dump {
        /// Print to stdout instead of writing the file.
        #[arg(long)]
        dry_run: bool,
    },
    /// Apply keybindings and reload settings.
    Apply {
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        ui::err(format!("{e:#}"));
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    // `completions` and `init` need no existing config; handle them first.
    if let Command::Completions { shell } = cli.command {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        return Ok(());
    }
    if let Command::Init { dir, force } = &cli.command {
        // A positional dir wins; else fall back to --config, then defaults.
        let target = dir.clone().or_else(|| cli.config.clone());
        return commands::init(target, *force, cli.yes);
    }

    let ctx = Ctx {
        config_dir: commands::resolve_config_dir(cli.config)?,
        profile: cli.profile,
        assume_yes: cli.yes,
    };

    match cli.command {
        Command::Apply {
            only,
            dry_run,
            skip_missing,
        } => commands::apply(&ctx, only.as_deref(), dry_run, skip_missing),
        Command::Status => commands::status(&ctx),
        Command::Diff { only } => commands::diff(&ctx, only.as_deref()),

        Command::Link { packages, dry_run } => commands::link(&ctx, &packages, dry_run),
        Command::Unlink { packages, dry_run } => commands::unlink(&ctx, &packages, dry_run),
        Command::Relink { packages, dry_run } => commands::relink(&ctx, &packages, dry_run),
        Command::Adopt {
            package,
            files,
            dry_run,
        } => commands::adopt(&ctx, &package, &files, dry_run),

        Command::Pkg { cmd } => dispatch_pkg(&ctx, cmd, false),
        Command::Brew { cmd } => dispatch_pkg(&ctx, cmd, true),

        Command::Macos { cmd } => match cmd {
            MacosCmd::Apply { only, dry_run } => {
                commands::macos_apply(&ctx, only.as_deref(), dry_run)
            }
            MacosCmd::Diff { only } => commands::macos_diff(&ctx, only.as_deref()),
            MacosCmd::Dump {
                domain,
                output,
                all,
                dry_run,
            } => commands::macos_dump(&ctx, &domain, output.as_deref(), all, dry_run),
            MacosCmd::Revert { domain, dry_run } => {
                commands::macos_revert(&ctx, domain.as_deref(), dry_run)
            }
        },
        Command::Keyboard { cmd } => match cmd {
            KeyboardCmd::Dump { dry_run } => commands::keyboard_dump(&ctx, dry_run),
            KeyboardCmd::Apply { dry_run } => commands::keyboard_apply(&ctx, dry_run),
        },

        Command::Profile => commands::profile_show(&ctx),
        Command::Doctor { fix } => commands::doctor(&ctx, fix),
        Command::Capture => commands::capture(&ctx),
        Command::Init { .. } | Command::Completions { .. } => unreachable!("handled above"),
    }
}

/// Dispatch a `pkg`/`brew` subcommand. When `pin_brew` is set, the provider
/// selector is forced to "brew" (used by the `brew` alias).
fn dispatch_pkg(ctx: &Ctx, cmd: PkgCmd, pin_brew: bool) -> anyhow::Result<()> {
    let pin = |p: Option<String>| {
        if pin_brew {
            Some(p.unwrap_or_else(|| "brew".to_string()))
        } else {
            p
        }
    };
    match cmd {
        PkgCmd::Apply {
            provider,
            dry_run,
            skip_missing,
        } => commands::pkg_apply(ctx, pin(provider).as_deref(), dry_run, skip_missing),
        PkgCmd::Diff { provider } => commands::pkg_diff(ctx, pin(provider).as_deref()),
        PkgCmd::Clean { provider, dry_run } => {
            commands::pkg_clean(ctx, pin(provider).as_deref(), dry_run)
        }
        PkgCmd::Dump { provider, dry_run } => {
            commands::pkg_dump(ctx, pin(provider).as_deref(), dry_run)
        }
        PkgCmd::List => commands::pkg_list(ctx),
    }
}
