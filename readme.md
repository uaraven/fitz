# fitz

`fitz` is a small toolset for working with FITS (astronomy image) files. It can compress and
decompress FITS files, debayer mosaic images, auto-stretch them, split them into per-channel
files, inspect headers and pixel statistics, and preview images.

I started fitz to quickly uncompress files created by NINA, because some of the tools and Siril
scripts have problems with compressed files; after a couple of days the project expanded into
what it is now.

## Components

The project is a Cargo workspace with three crates:

 - **[fitz-core](fitz-core)** — the reusable library: FITS I/O (with transparent
   tile-decompression), debayering, auto-stretch, per-channel splitting, header/pixel-stat
   inspection, header copying, and image resizing. Both frontends depend on it.
 - **[fitz](fitz-cli/readme.md)** — the command-line tool. See its
   [readme](fitz-cli/readme.md) for the full command and option reference.
 - **[FitSmith](fitsmith/readme.md)** — a desktop GUI frontend built with Slint. See its
   [readme](fitsmith/readme.md) for details.

## Building

```shell
cargo build --release           # build the whole workspace
cargo run -p fitz -- --help     # run the CLI
cargo run -p fitsmith           # run the GUI
```

The `edition = "2024"` crates need a recent stable Rust toolchain (install via
[rustup](https://rustup.rs/)).

### System dependencies

The `fitz` and `fitz-core` crates are pure Rust and build with no extra system libraries.
The **FitSmith** GUI, however, pulls in [Slint](https://slint.dev/), which links `fontconfig`
at build time and uses the platform's windowing/graphics stack at runtime. If you only need
the CLI, you can skip all of this and build just those crates:

```shell
cargo build -p fitz             # CLI only — no GUI system deps needed
```

To build the whole workspace (including FitSmith):

- **macOS** — no extra packages; everything Slint needs ships with the system. Build with the
  Xcode command-line tools installed (`xcode-select --install`).
- **Linux** — install the `fontconfig` development package (it provides `fontconfig.pc` for
  pkg-config, needed at build time). The runtime Wayland/X11/OpenGL libraries are loaded
  dynamically and are present on any normal desktop install.

  ```shell
  # Fedora / RHEL
  sudo dnf install fontconfig-devel

  # Debian / Ubuntu
  sudo apt install libfontconfig1-dev

  # Arch
  sudo pacman -S fontconfig
  ```

- **Windows** — no extra packages; build with the MSVC toolchain (install the "Desktop
  development with C++" workload from the Visual Studio Build Tools).

If FitSmith fails to build with a `pkg-config ... fontconfig was not found` error, the
`fontconfig` dev package above is what's missing.

## Note

This is a small personal project and as such it is not thouroughly tested and not optimized in
any way. Use at your own risk.

## License

MIT — see [LICENSE](LICENSE).

Note that the FitSmith GUI uses [Slint](https://slint.dev/) under the GPLv3 license; see the
[FitSmith readme](fitsmith/readme.md#slint-and-licensing) for details.

## AI Warning

I needed a quick and dirty tool to compress and uncompress fits files. Researching libraries, understanding FITS format and writing it myself would take time and I needed it now. The result is this tool is mostly vibe-coded with Claude Code. I review the code to make sure I understand what it does and I make changes where neccessary, but still most of the authorship goes to those anonymous heroes who write the code, on which Anthropic trains their models.

I understand the feelings a lot of people harbor towards AI-written code. I share a lot of these feelings, but, honestly, for a low-effort, low-impact and low-risk utility it kinda makes sense. I would spend at least a couple of weeks writing this or I could have what I need in two days.

Let's face it. AI isn't going anywhere (most likely). It's a new tool for us to use and it is a powerful tool. As long as we use it responsibly and own the outcomes I am going to treat it the same way as I treat compiler rewriting my code to improve performance.