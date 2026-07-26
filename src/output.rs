//! Output modes, result envelopes, and stdout/stderr discipline.
//!
//! The CLI supports three human-facing modes (normal, `--verbose`, `--quiet`) plus a
//! machine-readable `--json` mode. Progress and diagnostics always go to stderr so that
//! stdout stays script-safe; results go to stdout. JSON results are wrapped in a stable
//! envelope (`schema_version: 1`).

use std::io::IsTerminal;
use std::sync::OnceLock;

use serde_json::{Map, Value};

/// The JSON envelope schema version. Bump on breaking output changes.
pub const SCHEMA_VERSION: u32 = 1;

/// Exit codes with stable meanings for scripting.
pub mod exit {
    /// The command succeeded.
    pub const OK: i32 = 0;
    /// A recoverable/user error (bad input, unmet precondition).
    pub const RECOVERABLE: i32 = 1;
    /// A chain/RPC failure: the network could not be reached or returned an error, so the
    /// result is unknown (as opposed to a definitive on-chain "no").
    pub const CHAIN_FAILURE: i32 = 3;
}

/// Classifies an error for exit-code purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// A recoverable/user error (bad input or unmet precondition).
    Recoverable,
    /// A chain/RPC failure (status unknown, network unreachable, etc.).
    Chain,
}

/// A structured CLI error rendered as Problem / Why / Next-step.
#[derive(Debug)]
pub struct AppError {
    /// What went wrong (the headline).
    pub problem: String,
    /// Why it happened (optional context).
    pub why: Option<String>,
    /// What the user should do next (optional).
    pub next: Option<String>,
    /// Error classification for the process exit code.
    pub kind: ErrKind,
}

impl AppError {
    /// A recoverable error with just a problem statement.
    pub fn recoverable(problem: impl Into<String>) -> Self {
        Self {
            problem: problem.into(),
            why: None,
            next: None,
            kind: ErrKind::Recoverable,
        }
    }

    /// A chain/RPC failure with just a problem statement.
    pub fn chain(problem: impl Into<String>) -> Self {
        Self {
            problem: problem.into(),
            why: None,
            next: None,
            kind: ErrKind::Chain,
        }
    }

    /// Adds a "why" clause.
    pub fn why(mut self, why: impl Into<String>) -> Self {
        self.why = Some(why.into());
        self
    }

    /// Adds a "next step" clause.
    pub fn next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }

    /// The process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self.kind {
            ErrKind::Recoverable => exit::RECOVERABLE,
            ErrKind::Chain => exit::CHAIN_FAILURE,
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.problem)?;
        if let Some(why) = &self.why {
            write!(f, "\n  Why:  {why}")?;
        }
        if let Some(next) = &self.next {
            write!(f, "\n  Next: {next}")?;
        }
        Ok(())
    }
}

impl std::error::Error for AppError {}

/// Renders a top-level error to stderr (or a JSON error envelope in JSON mode) and returns
/// the exit code to use.
pub fn render_error(err: &anyhow::Error) -> i32 {
    let (code, app) = match err.downcast_ref::<AppError>() {
        Some(app) => (app.exit_code(), Some(app)),
        None => (exit::RECOVERABLE, None),
    };
    if is_json() {
        let mut env = Map::new();
        env.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
        env.insert("ok".into(), Value::Bool(false));
        env.insert("error".into(), Value::String(err.to_string()));
        if let Some(app) = app {
            if let Some(why) = &app.why {
                env.insert("why".into(), Value::String(why.clone()));
            }
            if let Some(next) = &app.next {
                env.insert("next".into(), Value::String(next.clone()));
            }
        }
        env.insert("exit_code".into(), Value::from(code));
        eprintln!(
            "{}",
            serde_json::to_string_pretty(&Value::Object(env)).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        eprintln!("Error: {err}");
    }
    code
}

/// The active output mode for this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Normal human output.
    Human,
    /// Machine-readable JSON.
    Json,
}

#[derive(Debug, Clone, Copy)]
struct Config {
    mode: Mode,
    verbose: bool,
    quiet: bool,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Initializes the global output configuration. Idempotent (first call wins).
pub fn init(json: bool, verbose: bool, quiet: bool) {
    let _ = CONFIG.set(Config {
        mode: if json { Mode::Json } else { Mode::Human },
        verbose,
        quiet,
    });
}

fn config() -> Config {
    CONFIG.get().copied().unwrap_or(Config {
        mode: Mode::Human,
        verbose: false,
        quiet: false,
    })
}

/// True when JSON output is selected.
pub fn is_json() -> bool {
    config().mode == Mode::Json
}

/// True when verbose output is selected.
pub fn is_verbose() -> bool {
    config().verbose
}

/// True when quiet output is selected.
pub fn is_quiet() -> bool {
    config().quiet
}

/// True when stderr is an interactive terminal (used to gate prompts).
pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal() && std::io::stdin().is_terminal()
}

/// Prints a progress/diagnostic line to stderr (suppressed in quiet or JSON mode).
pub fn progress(msg: impl AsRef<str>) {
    let cfg = config();
    if cfg.quiet || cfg.mode == Mode::Json {
        return;
    }
    eprintln!("{}", msg.as_ref());
}

/// Prints a warning to stderr (always shown unless JSON mode, where it is suppressed to
/// keep stdout/stderr machine-clean; callers should surface warnings via the envelope).
pub fn warn(msg: impl AsRef<str>) {
    if config().mode == Mode::Json {
        return;
    }
    eprintln!("warning: {}", msg.as_ref());
}

/// A structured, mode-aware result for a command.
///
/// Populate it with fields, then call [`Report::emit`]. In human mode it prints a titled
/// block; in JSON mode it prints a single envelope object; in quiet mode it prints only
/// fields marked primary.
#[derive(Debug)]
pub struct Report {
    kind: String,
    title: String,
    lines: Vec<Line>,
    notes: Vec<String>,
    data: Map<String, Value>,
}

#[derive(Debug)]
struct Line {
    label: String,
    value: String,
    primary: bool,
}

impl Report {
    /// Creates a new report with a machine `kind` and a human `title`.
    pub fn new(kind: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            title: title.into(),
            lines: Vec::new(),
            notes: Vec::new(),
            data: Map::new(),
        }
    }

    /// Adds a human-visible line and a JSON field (string value).
    pub fn field(
        &mut self,
        label: impl Into<String>,
        value: impl Into<String>,
        json_key: impl Into<String>,
    ) -> &mut Self {
        let value = value.into();
        self.data
            .insert(json_key.into(), Value::String(value.clone()));
        self.lines.push(Line {
            label: label.into(),
            value,
            primary: false,
        });
        self
    }

    /// Adds a human-visible line and a JSON field with a distinct raw JSON value (e.g. a
    /// number), so the human display can be pretty while JSON stays exact.
    pub fn field_json(
        &mut self,
        label: impl Into<String>,
        human: impl Into<String>,
        json_key: impl Into<String>,
        json_value: Value,
    ) -> &mut Self {
        self.data.insert(json_key.into(), json_value);
        self.lines.push(Line {
            label: label.into(),
            value: human.into(),
            primary: false,
        });
        self
    }

    /// Marks the most recently added line as "primary" (shown even in quiet mode).
    pub fn primary(&mut self) -> &mut Self {
        if let Some(last) = self.lines.last_mut() {
            last.primary = true;
        }
        self
    }

    /// Adds a JSON-only field (not shown in human output).
    pub fn json_only(&mut self, key: impl Into<String>, value: Value) -> &mut Self {
        self.data.insert(key.into(), value);
        self
    }

    /// Adds a trailing human note (also captured under `notes` in JSON).
    pub fn note(&mut self, text: impl Into<String>) -> &mut Self {
        self.notes.push(text.into());
        self
    }

    /// Emits the report according to the active output mode.
    pub fn emit(self) {
        let cfg = config();
        match cfg.mode {
            Mode::Json => {
                let mut env = Map::new();
                env.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
                env.insert("ok".into(), Value::Bool(true));
                env.insert("kind".into(), Value::String(self.kind));
                for (k, v) in self.data {
                    env.insert(k, v);
                }
                if !self.notes.is_empty() {
                    env.insert(
                        "notes".into(),
                        Value::Array(self.notes.into_iter().map(Value::String).collect()),
                    );
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&Value::Object(env))
                        .unwrap_or_else(|_| "{}".to_string())
                );
            }
            Mode::Human if cfg.quiet => {
                for line in self.lines.iter().filter(|l| l.primary) {
                    println!("{}", line.value);
                }
            }
            Mode::Human => {
                println!("{}", self.title);
                // Labels are printed with a trailing colon, so the column has to be one
                // wider than the longest label or that label overflows it.
                let width = self.lines.iter().map(|l| l.label.len()).max().unwrap_or(0) + 1;
                for line in &self.lines {
                    println!("  {:<width$}  {}", format!("{}:", line.label), line.value);
                }
                for note in &self.notes {
                    println!("\n{note}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_field_is_recorded() {
        let mut r = Report::new("test", "Test");
        r.field("Launcher", "0xabc", "launcher_id");
        assert_eq!(r.data.get("launcher_id").unwrap(), "0xabc");
    }

    #[test]
    fn json_only_is_not_a_line() {
        let mut r = Report::new("test", "Test");
        r.json_only("hidden", Value::from(42));
        assert!(r.lines.is_empty());
        assert_eq!(r.data.get("hidden").unwrap(), 42);
    }
}
