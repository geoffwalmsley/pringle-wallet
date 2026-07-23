//! Interactive confirmation prompts for destructive mainnet actions.
//!
//! Prompts are only shown when stderr/stdin are interactive terminals. Non-interactive
//! invocations (scripts, CI) keep their existing behavior and never block. `--yes` skips
//! the prompt in all cases.

use std::io::Write;

use anyhow::Result;

use crate::output;

/// A preview of an action requiring confirmation before it touches mainnet.
#[derive(Debug, Default)]
pub struct ActionPreview {
    /// The action verb/name, e.g. "Mint NFT".
    pub action: String,
    /// Ordered detail lines to show (label, value).
    pub details: Vec<(String, String)>,
}

impl ActionPreview {
    /// Starts a preview for a named action.
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            details: Vec::new(),
        }
    }

    /// Adds a detail line.
    pub fn detail(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.details.push((label.into(), value.into()));
        self
    }

    /// Confirms the action, returning `Ok(true)` to proceed.
    ///
    /// Behavior:
    /// - `assume_yes` (from `--yes`): always proceeds (a one-line preview goes to stderr).
    /// - interactive terminal: prints the preview and asks `Proceed? [y/N]`.
    /// - non-interactive: proceeds without prompting (preserves existing script behavior),
    ///   emitting the preview to stderr for the record.
    pub fn confirm(&self, assume_yes: bool) -> Result<bool> {
        // Always show the preview (to stderr, so stdout stays clean) unless quiet/JSON.
        self.print_preview();

        if assume_yes || !output::stderr_is_tty() {
            return Ok(true);
        }

        eprint!("Proceed? [y/N] ");
        std::io::stderr().flush().ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let answer = input.trim().to_lowercase();
        Ok(answer == "y" || answer == "yes")
    }

    fn print_preview(&self) {
        if output::is_quiet() || output::is_json() {
            return;
        }
        eprintln!("About to: {} (mainnet)", self.action);
        let width = self.details.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
        for (label, value) in &self.details {
            eprintln!("  {:<width$}  {}", format!("{label}:"), value);
        }
    }
}
