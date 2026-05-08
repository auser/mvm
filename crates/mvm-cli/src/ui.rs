use colored::Colorize;
use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    io::IsTerminal,
    os::fd::AsRawFd,
    time::{Duration, Instant},
};
// ---------------------------------------------------------------------------
// Verbosity
// ---------------------------------------------------------------------------

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Enable verbose `[mvm]` chatter (info/success/warn/step). Errors are
/// always printed regardless. Called once at CLI startup based on
/// `--verbose`/`--debug` or the presence of `RUST_LOG`.
pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// Whether `[mvm]` chatter is currently enabled.
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

const BRAILLE_TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠋"];

// ---------------------------------------------------------------------------
// Colored message helpers
// ---------------------------------------------------------------------------

fn prefix() -> String {
    "[mvm]".bold().cyan().to_string()
}

/// Print an informational message: [mvm] message
pub fn info(msg: &str) {
    println!("{} {}", prefix(), msg);
}

/// Print a success message: [mvm] message (in green)
pub fn success(msg: &str) {
    println!("{} {}", prefix(), msg.green());
}

/// Print an error message: [mvm] ERROR: message (in red).
pub fn error(msg: &str) {
    eprintln!("{} {}", "[mvm]".bold().red(), msg.red());
}

/// Print a warning message: [mvm] message (in yellow)
pub fn warn(msg: &str) {
    println!("{} {}", prefix(), msg.yellow());
}

/// Print a numbered step: [mvm] Step n/total: message
pub fn step(n: u32, total: u32, msg: &str) {
    println!(
        "\n{} {} {}",
        prefix(),
        format!("Step {}/{}:", n, total).bold().yellow(),
        msg,
    );
}

/// Print a progress / chatter message that's only useful when
/// troubleshooting (e.g. "auto-starting dev VM…"). Suppressed by default;
/// shown when `--verbose`/`--debug` is passed or `RUST_LOG` is set.
pub fn progress(msg: &str) {
    if !is_verbose() {
        return;
    }
    println!("{} {}", prefix(), msg);
}

// ---------------------------------------------------------------------------
// Banner
// ---------------------------------------------------------------------------

/// Print a green bold banner box.
pub fn banner(lines: &[&str]) {
    let width = lines.iter().map(|l| l.len()).max().unwrap_or(0) + 4;
    let rule = "=".repeat(width);

    println!();
    println!("{}", rule.bold().green());
    for line in lines {
        let pad = width - line.len() - 4;
        println!(
            "{}",
            format!("  {}{}  ", line, " ".repeat(pad)).bold().green()
        );
    }
    println!("{}", rule.bold().green());
    println!();
}

// ---------------------------------------------------------------------------
// Status table
// ---------------------------------------------------------------------------

/// Print the status header.
pub fn status_header() {
    println!("{}", "mvmctl status".bold());
    println!("{}", "-------------".dimmed());
}

/// Print a status line with a bold label and a colored value.
/// Recognized values: "Running", "Stopped", "Not running", etc.
pub fn status_line(label: &str, value: &str) {
    let colored_value = if value.starts_with("Running") {
        value.green().to_string()
    } else if value == "Stopped" {
        value.yellow().to_string()
    } else if value.starts_with("Not ") || value == "-" {
        value.dimmed().to_string()
    } else if value.starts_with("Starting") {
        value.yellow().to_string()
    } else {
        value.to_string()
    };

    println!("{} {}", format!("{:<14}", label).bold(), colored_value);
}

// ---------------------------------------------------------------------------
// Interactive prompts
// ---------------------------------------------------------------------------

/// Show an interactive confirmation prompt. Returns true if confirmed.
pub fn confirm(msg: &str) -> bool {
    inquire::Confirm::new(msg)
        .with_default(false)
        .prompt()
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Spinners
// ---------------------------------------------------------------------------

// /// Create and start a spinner with the given message.
// /// Call `.finish_with_message()` or `.finish_and_clear()` when done.
// pub fn spinner(msg: &str) -> ProgressBar {
//     let pb = ProgressBar::new_spinner();
//     pb.set_style(
//         ProgressStyle::default_spinner()
//             .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
//             .template("{spinner:.cyan} {msg}")
//             .expect("invalid spinner template"),
//     );
//     pb.set_message(msg.to_string());
//     pb.enable_steady_tick(std::time::Duration::from_millis(80));
//     pb
// }

pub struct Spinner {
    pb: Option<ProgressBar>,
    start: Instant,
    target: String,
    quiet: bool,
    _echo_guard: Option<EchoGuard>,
}

impl Spinner {
    /// Start a new spinner. Label is the action verb (e.g., "Creating"),
    /// target is the object name (e.g., "mybox").
    pub fn start(label: &str, target: &str) -> Self {
        let is_tty = std::io::stderr().is_terminal();
        let (pb, echo_guard) = if is_tty {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .tick_strings(BRAILLE_TICKS)
                    .template(&format!("   {{spinner}} {:<12} {{msg}}", label))
                    .unwrap(),
            );
            pb.set_message(target.to_string());
            pb.enable_steady_tick(Duration::from_millis(80));
            (Some(pb), EchoGuard::acquire())
        } else {
            (None, None)
        };

        Self {
            pb,
            start: Instant::now(),
            target: target.to_string(),
            quiet: false,
            _echo_guard: echo_guard,
        }
    }

    /// Create a no-op spinner that produces no output.
    pub fn quiet() -> Self {
        Self {
            pb: None,
            start: Instant::now(),
            target: String::new(),
            quiet: true,
            _echo_guard: None,
        }
    }

    /// Finish with success. Shows `✓ <past_tense> <target> (duration)`.
    pub fn finish_success(self, past_tense: &str) {
        if let Some(pb) = self.pb {
            pb.finish_and_clear();
        }

        if !self.quiet {
            let elapsed = self.start.elapsed();
            let duration = if elapsed.as_millis() > 500 {
                format!(" ({})", format_duration(elapsed))
            } else {
                String::new()
            };

            eprintln!(
                "   {} {:<12} {}{}",
                style("✓").green(),
                past_tense,
                self.target,
                style(duration).dim()
            );
        }
    }

    /// Finish and clear entirely — no output remains on screen.
    ///
    /// Used on both success and failure paths: errors are presented by
    /// the top-level error renderer, so the spinner has no failure
    /// state of its own.
    pub fn finish_clear(self) {
        if let Some(pb) = self.pb {
            pb.finish_and_clear();
        }
    }
}

/// RAII guard that disables terminal echo while held.
///
/// Prevents stray keypresses (e.g. Enter) from injecting newlines that
/// desync indicatif's cursor tracking, which causes ghost lines.
struct EchoGuard {
    original: libc::termios,
    fd: i32,
}

impl EchoGuard {
    /// Disable terminal echo on stdin. Returns `None` if stdin is not a TTY.
    fn acquire() -> Option<Self> {
        if !std::io::stdin().is_terminal() {
            return None;
        }

        let fd = std::io::stdin().as_raw_fd();
        let mut original: libc::termios = unsafe { std::mem::zeroed() };

        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return None;
        }

        let mut modified = original;
        modified.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &modified) } != 0 {
            return None;
        }

        Some(Self { original, fd })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        // Flush any keypresses that accumulated while echo was off,
        // so they don't spill into the shell prompt after we restore.
        unsafe {
            libc::tcflush(self.fd, libc::TCIFLUSH);
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// Format a duration for display.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let mins = secs as u64 / 60;
        let remaining = secs as u64 % 60;
        format!("{mins}m{remaining}s")
    }
}
