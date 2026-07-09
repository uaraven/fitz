//! CLI-only I/O helpers: confirming overwrite of an existing output file, and
//! the `--verbose` progress/step printing shared by every command wrapper.

use std::path::Path;

use anyhow::{Result, bail};
use fitz_core::fits_image::LoadRgbNotice;

use crate::terminal::print_warning;

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
/// (matching the old non-interactive guard).
fn confirm_overwrite(output: &Path) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        bail!("{} already exists — use -y to overwrite", output.display());
    }

    // Hold the lock across the whole prompt/answer exchange.
    let _guard = PROMPT_LOCK.lock().unwrap();
    print!("{} already exists — overwrite? [y/N] ", output.display());
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}

/// Print the `input -> output` mapping when verbose mode is enabled.
pub fn print_progress(verbose: bool, input: &Path, output: &Path) {
    if verbose {
        println!("{} -> {}", input.display(), output.display());
    }
}

/// Print the name of an operation (reading, debayering, …) when verbose mode is
/// enabled.
pub fn print_step(verbose: bool, step: &str) {
    if verbose {
        println!("  {step}");
    }
}

/// Report how `load_rgb` handled demosaicing for `input`: a plain verbose step
/// when it demosaiced, or a note/warning when it decided the image was
/// already debayered. Shared by the `debayer` and `stretch` commands, which
/// both call `load_rgb` under the hood.
pub fn print_load_rgb_notice(verbose: bool, input: &Path, notice: LoadRgbNotice) {
    match notice {
        LoadRgbNotice::AlreadyDebayeredRgbCube => {
            println!(
                "{}: already debayered — skipping debayer step",
                input.display()
            );
        }
        LoadRgbNotice::AlreadyDebayeredMono => {
            print_warning(&format!(
                "{}: 1-channel image with no BAYERPAT header — treating it as an already-debayered \
                 monochrome image",
                input.display()
            ));
        }
        LoadRgbNotice::Demosaiced => {
            print_step(verbose, "debayering");
        }
    }
}
