# Design: SIMD investigation for hot pixel loops

Target: fitz v0.2.0 (investigation + targeted wins) · Crates touched: `libfitz`.

## 1. The question in the spec

> Does Rayon support SIMD? Investigate SIMD support for speeding up processing.

## 2. Direct answer: Rayon and SIMD are orthogonal

**Rayon does not do SIMD.** Rayon is a *thread-level* data-parallelism library: it splits an
iterator's work across CPU cores. SIMD (Single Instruction, Multiple Data) is *within a
single core* — one instruction operating on a vector of lanes (e.g. 8×`f32` in one AVX2 op).
They compose: Rayon spreads chunks across cores, and each core's inner loop can additionally
be vectorised. Neither implies the other.

The codebase already uses Rayon heavily for the thread-level axis (`par_iter` in
`fits_image.rs` demosaic/plane build, `info.rs` stats, `stars.rs` detection). So the SIMD
question is really: **can the inner per-pixel loops be vectorised, and is it worth it?**

## 3. Three routes to SIMD in Rust, ranked

1. **Autovectorisation (compiler-driven) — do this first, lowest effort.**
   LLVM already vectorises simple, branch-free, contiguous `f64`/`f32`/integer loops *if*
   the code and build flags let it. The build currently fights this:
   - The release profile is `opt-level = 'z'` (size), which suppresses much autovectorisation.
     `libfitz` is already overridden to `opt-level = 2` (`Cargo.toml`) precisely because "'z'
     hurts libfitz's own tight per-pixel loops." **`opt-level = 3` typically unlocks more
     autovectorisation than `2`** — worth measuring for `libfitz` specifically.
   - By default `rustc` targets a baseline CPU (SSE2 only on x86-64). Building with
     `-C target-cpu=native` (or `target-feature=+avx2,+fma`) lets LLVM emit wider vectors.
     This is a distribution trade-off (a `native` binary isn't portable), so it belongs behind
     an opt-in profile / env, not the default release build.
   The cheapest experiment: measure the hot paths at `opt-level = 3` and with
   `RUSTFLAGS="-C target-cpu=native"`, and see how much the compiler already gives for free.

2. **`std::simd` (portable SIMD) — the right explicit tool, but nightly.**
   `core::simd` (`f64x8`, etc.) is ergonomic and portable across ISAs, but is **unstable —
   nightly only**. The project uses stable (`edition = "2024"`, CLAUDE.md notes "recent
   stable toolchain"). Adopting `std::simd` would pin `libfitz` to nightly, which is a
   significant policy change for a "low-effort, low-risk, largely AI-authored" tool. Not
   recommended unless a measured hot path justifies it.

3. **A stable SIMD crate (`wide`, `pulp`, `simba`) — explicit SIMD on stable.**
   `wide` gives `f64x4`/`f32x8` types on stable with no unsafe. `pulp` offers runtime
   feature dispatch (compile once, pick AVX2/AVX-512 at runtime — portable *and* fast). If a
   hot path is proven worth hand-vectorising, `pulp` is the best fit: stable, safe, runtime
   dispatch so the shipped binary stays portable. Adds a dependency, which the project keeps
   deliberately lean, so gate it on a real measurement.

## 4. Candidate hot loops (where SIMD could actually pay)

Profile before touching any of these (`cargo build --profile profiling` already exists). The
per-pixel, arithmetic-heavy, branch-light loops are the plausible wins:

- **`scaled_pixels` / `ImageData::scaled_values`** — `value * bscale + bzero` over every
  pixel. Textbook FMA autovectorisation; likely already vectorised at `opt-level 2/3`. Check
  the asm before assuming a win is available to take.
- **`stretch.rs`** — the per-pixel stretch math; CLAUDE.md already flags these loops as the
  reason `libfitz` opts out of `opt-level z`. Prime candidate; measure at `opt-level 3`.
- **`detection_plane` green super-pixel averaging** (`fits_image.rs:212`) — strided gathers
  (two green sites per 2×2 cell); strided access vectorises poorly, lower priority.
- **`info.rs` statistics** — min/max/mean/sum reductions and the histogram. Reductions
  autovectorise well; the histogram has a scatter (bucket increment) that does not. The RGB
  luminance reduction (from the RGB design) is a new `0.21R+0.72G+0.07B` FMA loop — a clean
  vectorisation target, worth building vector-friendly from the start.
- **Demosaic** — largely inside the `bayer` dependency; out of our control.

## 5. Recommended plan (low-risk, measurement-driven)

Consistent with the readme's "intentionally low-effort, low-risk" stance:

1. **Establish a benchmark.** Add a small `criterion` bench (or reuse the profiling profile
   with a timing harness) over `test-data` frames for: full stretch, `pixel_stats`, star
   detection, and RGB luminance reduction. Without numbers, none of this is decidable.
2. **Try the free wins first, in order:**
   a. Bump `libfitz` to `opt-level = 3` in the release profile override; measure.
   b. Add an opt-in `profiling`/`native` profile (or document `RUSTFLAGS=-C target-cpu=native`)
      and measure the ceiling autovectorisation reaches with AVX2/FMA available.
3. **Only if a specific loop is still hot and provably not autovectorised**, hand-vectorise
   *that one loop* with `pulp` (stable, safe, runtime dispatch — keeps the shipped binary
   portable). Keep it behind the existing pure-function boundaries so it stays unit-testable
   against the scalar result.
4. **Do not** adopt nightly `std::simd` for this tool.
5. **Order relative to the caching work:** land stats caching first. It removes the *most*
   redundant computation (star detection on every debayer/stretch toggle); SIMD only speeds
   up the computation that genuinely has to happen. Caching is the bigger, cheaper win.

## 6. Correctness & portability guardrails

- Floating-point vectorisation can reorder reductions, changing the last ULP of a sum. The
  SHA-256 regression tests pin exact output bytes for some paths — a reordered `f64` sum
  could shift a rendered pixel and break them. Any SIMD/`opt-level`/`target-cpu` change must
  run `cargo test --workspace` and, if a regression fixture shifts, the change must be
  understood (a legitimate rounding difference) before a fixture is re-baselined — never
  re-baseline blindly.
- `target-cpu=native` binaries crash with SIGILL on older CPUs. Keep `native` opt-in; ship
  the default portable build, or use `pulp`'s runtime dispatch if we want AVX2 in a portable
  binary.
- Keep every vectorised routine behind the same pure function signature and add a test
  asserting it matches the scalar implementation on real data (per CLAUDE.md's "add unit
  tests working on real data").

## 7. Deliverable for this spec item

This is fundamentally an **investigation** item. The concrete deliverables:

1. This finding: Rayon ≠ SIMD; they compose (Rayon across cores already in use, SIMD within
   a core is the open lever).
2. A benchmark harness over `test-data` so future SIMD/opt-level changes are measurable.
3. The cheap, portable experiments (`opt-level = 3`, opt-in `target-cpu=native`) with numbers.
4. A go/no-go on hand-vectorising the hottest surviving loop with `pulp`, decided by the
   benchmark — not committed to in advance.

## 8. Open questions / decisions

1. **Is any of this worth it for typical use?** The GUI decodes one frame per selection;
   stats caching may make per-frame latency a non-issue. SIMD matters most for the *batch*
   paths (Analytics/Star-metrics over hundreds of frames). Prioritise vectorising what the
   batch paths touch (`pixel_stats`, detection, luminance) over interactive single-frame math.
2. **Portable-but-fast vs. simple-but-baseline.** Decide whether shipping AVX2 via `pulp`
   runtime dispatch is worth one dependency, or whether an opt-in `target-cpu=native` build
   for power users is enough. Recommend the latter until a benchmark says otherwise.
