---
name: fitz-review
description: Review the fitz Rust codebase for improvements — remove duplication and overly complex code, and flag performance and security bugs — then verify all tests pass. Use whenever the user asks to "review the code", do a code review, or clean up / refactor the codebase.
---

# fitz code review

Review the `fitz` codebase for concrete, actionable improvements and, when asked, apply
safe refactors. The goal is cleaner, faster, safer code — not churn. Prefer a few
high-confidence findings over a long speculative list.

Follow the repo's ethos (see `CLAUDE.md` / `readme.md` "AI Warning"): this tool is
intentionally low-ceremony. Favor pragmatic changes; do not add heavy abstractions.

## Scope

By default, review what changed on the current branch versus `main`. If the working tree
is clean and the branch matches `main`, review the whole crate. If the user names specific
files or a command (e.g. "review the debayer path"), scope to that.

```shell
git diff main...HEAD --stat        # what changed on this branch
git status                          # uncommitted work
```

## What to look for

Go through the code with these lenses, in priority order:

1. **Correctness / security bugs** (highest priority)
   - Integer overflow/underflow in pixel-index and size math (image dims, `NAXIS*`,
     tile sizes, `BSCALE`/`BZERO` scaling). FITS headers are untrusted input — a hostile
     or malformed file must not cause panics, OOB slicing, or huge allocations.
   - Unchecked allocations sized from header values (guard before `Vec::with_capacity` /
     `vec![0; n]` on attacker-controlled `n`).
   - `unwrap()`/`expect()`/`panic!`/array indexing on paths reachable from file input;
     prefer `?` with `anyhow` context. Slice indexing that can go out of bounds.
   - Path handling for `-o`/output derivation (no unintended overwrite/traversal).
   - Silent truncation or precision loss in numeric casts (`as` between int/float widths).

2. **Duplication** — the top refactor target per `CLAUDE.md` ("Avoid code duplication").
   Look for logic repeated across the per-command modules (`compress.rs`, `decompress.rs`,
   `debayer.rs`, `stretch.rs`, `split_channel.rs`, `info.rs`) that belongs in the shared
   plumbing (`fits_image.rs`, `main.rs` helpers like `process_files` / path derivation, or
   `options.rs`). Verify new code reuses `find_image_hdu` / `load_rgb` / `resolve_cfa` /
   `scaled_pixels` rather than re-implementing them.

3. **Overly complex code** — deep nesting, long functions doing several things, convoluted
   control flow, needless intermediate allocations or clones. Suggest the simplest form
   that a reader of the surrounding code would write.

4. **Performance** — unnecessary buffer copies/clones of image data, per-pixel work that
   could be hoisted, allocating in hot loops, reading whole files when streaming would do.
   Image buffers are large; copies matter. Don't micro-optimize cold paths.

5. **Doc comments (`cargo doc`)** — every `pub` function, struct, and field should have a
   doc comment explaining *what it's for and how to use it* (purpose, arguments, return
   value, notable invariants like "untrusted input" or "panics if..."), not a restatement of
   the implementation ("loops over pixels and calls resolve_cfa" is not useful; "resolves the
   Bayer pattern from the header or an override" is). Flag: missing docs on public API,
   docs that describe the how instead of the why/what, and docs that have drifted from what
   the function actually does.

6. **Idiom & consistency** — match the surrounding style (naming, error handling with
   `anyhow`, `Cow` usage, verbose-gating via `print_progress`/`print_step`). Run
   `cargo clippy` and treat its warnings as findings.

## Verify tests — required, every run

Tests must be correct and green. This is not optional.

```shell
cargo test          # all tests, including cargo test --bin=fitz unit tests
cargo clippy        # lint findings feed into the review
```

- If any test fails, that is the first thing to report and (when applying fixes) to fix.
- Check that tests actually assert meaningful behavior on real data (fixtures in
  `test-data/` via `test_support`), not tautologies. Flag tests that would pass even if the
  code were broken.
- Any code change you apply must keep `cargo test` green. Re-run after changes.
- If you add or change behavior, add unit tests working on real data, per `CLAUDE.md`.

## How to report

Group findings by the lenses above, most severe first. For each: the file:line, a one-line
statement of the problem, why it matters, and a concrete fix. Distinguish **bugs** (should
fix) from **suggestions** (optional). Keep it tight.

If the user asked you to apply fixes (or it's clearly implied), make the safe,
high-confidence changes, reusing existing helpers and refactoring rather than duplicating.
After editing: run `cargo test` (and `cargo clippy`), report results, and update
`readme.md` if any command-line parameter or behavior changed (per `CLAUDE.md`).

Do not commit or push unless the user asks.
