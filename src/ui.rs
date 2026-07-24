//! Terminal UX: colored logging, confirmation prompts, and result summaries.
//! Port of the `info/warn/err/ok/ask` helpers from the original `utils.sh`.

use owo_colors::{OwoColorize, Stream::Stdout};
use std::io::{self, IsTerminal, Write};

/// `==>` blue informational line.
pub fn info(msg: impl AsRef<str>) {
    println!(
        "{} {}",
        "==>".if_supports_color(Stdout, |t| t.blue().bold().to_string()),
        msg.as_ref()
    );
}

/// `==>[!]` yellow warning line.
pub fn warn(msg: impl AsRef<str>) {
    println!(
        "{} {}",
        "==>[!]".if_supports_color(Stdout, |t| t.yellow().bold().to_string()),
        msg.as_ref()
    );
}

/// `==>[x]` red error line (stderr).
pub fn err(msg: impl AsRef<str>) {
    eprintln!(
        "{} {}",
        "==>[x]".if_supports_color(Stdout, |t| t.red().bold().to_string()),
        msg.as_ref()
    );
}

/// `==>[ok]` green success line.
pub fn ok(msg: impl AsRef<str>) {
    println!(
        "{} {}",
        "==>[ok]".if_supports_color(Stdout, |t| t.green().bold().to_string()),
        msg.as_ref()
    );
}

/// A dimmed detail line, indented under the current step.
pub fn detail(msg: impl AsRef<str>) {
    println!(
        "    {}",
        msg.as_ref()
            .if_supports_color(Stdout, |t| t.dimmed().to_string())
    );
}

/// A section heading.
pub fn heading(msg: impl AsRef<str>) {
    println!(
        "\n{}",
        msg.as_ref()
            .if_supports_color(Stdout, |t| t.bold().underline().to_string())
    );
}

/// y/N confirmation prompt. Returns false on EOF / non-tty unless `assume_yes`.
pub fn ask(prompt: &str, assume_yes: bool) -> bool {
    if assume_yes {
        return true;
    }
    if !io::stdin().is_terminal() {
        return false;
    }
    loop {
        print!(
            "{} [y/N]: ",
            prompt.if_supports_color(Stdout, |t| t.yellow().bold().to_string())
        );
        let _ = io::stdout().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return false; // EOF
        }
        match line.trim() {
            "y" | "Y" | "yes" | "Yes" => return true,
            "n" | "N" | "no" | "No" | "" => return false,
            _ => err("Please answer y or n."),
        }
    }
}

/// One line's worth of change status, used by `diff`/`apply` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Already in the desired state.
    Unchanged,
    /// Would change / did change.
    Changed,
    /// Failed to apply.
    Failed,
    /// Skipped (e.g. missing optional tool).
    Skipped,
}

impl Status {
    pub fn glyph(self) -> String {
        match self {
            Status::Unchanged => "="
                .if_supports_color(Stdout, |t| t.dimmed().to_string())
                .to_string(),
            Status::Changed => "~"
                .if_supports_color(Stdout, |t| t.yellow().to_string())
                .to_string(),
            Status::Failed => "x"
                .if_supports_color(Stdout, |t| t.red().bold().to_string())
                .to_string(),
            Status::Skipped => "-"
                .if_supports_color(Stdout, |t| t.dimmed().to_string())
                .to_string(),
        }
    }
}

/// Accumulates per-item outcomes and renders a final summary.
#[derive(Debug, Default)]
pub struct Summary {
    pub changed: usize,
    pub unchanged: usize,
    pub failed: usize,
    pub skipped: usize,
    failures: Vec<String>,
}

impl Summary {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an outcome and print the item line.
    pub fn record(&mut self, status: Status, label: &str) {
        match status {
            Status::Changed => self.changed += 1,
            Status::Unchanged => self.unchanged += 1,
            Status::Failed => {
                self.failed += 1;
                self.failures.push(label.to_string());
            }
            Status::Skipped => self.skipped += 1,
        }
        println!("  {} {}", status.glyph(), label);
    }

    /// Fold another summary into this one (for combining stages).
    pub fn merge(&mut self, other: Summary) {
        self.changed += other.changed;
        self.unchanged += other.unchanged;
        self.failed += other.failed;
        self.skipped += other.skipped;
        self.failures.extend(other.failures);
    }

    pub fn any_failed(&self) -> bool {
        self.failed > 0
    }

    /// Render the closing summary line.
    pub fn render(&self) {
        heading("Summary");
        println!(
            "  {} changed   {} unchanged   {} failed   {} skipped",
            self.changed
                .if_supports_color(Stdout, |t| t.yellow().to_string()),
            self.unchanged,
            self.failed
                .if_supports_color(Stdout, |t| t.red().to_string()),
            self.skipped,
        );
        for f in &self.failures {
            err(format!("failed: {f}"));
        }
    }
}
