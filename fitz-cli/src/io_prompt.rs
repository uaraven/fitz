//! CLI-only I/O helpers: confirming overwrite of an existing output file, and
//! the `--verbose` progress/step printing shared by every command wrapper.

use std::path::Path;

use anyhow::{Result, bail};

/// Serializes overwrite prompts so parallel batch runs don't interleave their
/// questions and answers on the shared terminal.
static PROMPT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Ensure `output` may be written. If it already exists and the user didn't
/// pass `--yes`, ask whether to overwrite it (when running interactively) and
/// bail if the answer is no.
pub fn ensure_can_write(output: &Path, assume_yes: bool) -> Result<()> {
    if !output.exists() || assume_yes {
        return Ok(());
    }
    if confirm_overwrite(output)? {
        Ok(())
    } else {
        bail!("{} already exists — skipped", output.display());
    }
}

/// Prompt on the terminal whether to overwrite an existing `output`. When stdin
/// isn't a terminal there's no one to ask, so refuse and point at `--yes`
/// (matching the old non-interactive guard). Also refuses under `cfg(test)`
/// unconditionally: an automated test must never block on real keyboard
/// input, but `is_terminal()` alone can't guarantee that — a test binary run
/// from an interactive shell (or an IDE's test runner) can inherit a real TTY
/// on stdin, which would otherwise make this hang waiting for an answer
/// nobody is typing.
fn confirm_overwrite(output: &Path) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};

    if cfg!(test) || !std::io::stdin().is_terminal() {
        bail!("{} already exists — use -y to overwrite", output.display());
    }

    // Hold the lock across the whole prompt/answer exchange.
    let _guard = PROMPT_LOCK.lock().unwrap();
    print!("{} already exists — overwrite? [y/N] ", output.display());
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(parse_overwrite_answer(&answer))
}

/// Parse a typed response to the overwrite prompt: only an explicit yes
/// (case-insensitive `y`/`yes`) counts as consent; anything else — including
/// a blank line — defaults to "no".
fn parse_overwrite_answer(answer: &str) -> bool {
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

/// Print the `input -> output` mapping when verbose mode is enabled.
pub fn print_progress(input: &Path, output: &Path) {
    println!("{} -> {}", input.display(), output.display());
}

/// Print the name of an operation (reading, debayering, …) when verbose mode is
/// enabled.
pub fn print_step(verbose: bool, step: &str) {
    if verbose {
        println!("  {step}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_overwrite_answer_accepts_only_an_explicit_yes() {
        for yes in ["y", "Y", "yes", "Yes", "YES", " y \n"] {
            assert!(parse_overwrite_answer(yes), "{yes:?} should be yes");
        }
        for no in ["", "\n", "n", "N", "no", "maybe", "yesplease"] {
            assert!(!parse_overwrite_answer(no), "{no:?} should be no");
        }
    }

    #[test]
    fn ensure_can_write_allows_a_new_output() {
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("new.fits");
        ensure_can_write(&output, false).unwrap();
    }

    #[test]
    fn ensure_can_write_allows_an_existing_output_with_assume_yes() {
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("existing.fits");
        std::fs::write(&output, b"data").unwrap();
        ensure_can_write(&output, true).unwrap();
    }

    #[test]
    fn ensure_can_write_refuses_an_existing_output_without_yes() {
        // `confirm_overwrite` forces its non-interactive branch under
        // `cfg(test)`, so this must fail instantly rather than blocking on a
        // terminal prompt — regardless of whether this test process itself
        // inherited a real TTY on stdin.
        let tmp = TempDir::new().unwrap();
        let output = tmp.path().join("existing.fits");
        std::fs::write(&output, b"data").unwrap();
        let err = ensure_can_write(&output, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }
}
