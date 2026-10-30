# Building FitSmith

This covers building, running and packaging FitSmith from source, and the licensing
implications of doing so. For what FitSmith does and how to install it, see the
[readme](readme.md).

## Building and running

FitSmith is part of the `fitz` Cargo workspace:

```shell
cargo run -p fitsmith                 # run the GUI
cargo build -p fitsmith --release     # build a release binary
```

You can also pass files or folders on the command line to seed the working set:

```shell
cargo run -p fitsmith -- path/to/images/
```

See the [workspace readme](../readme.md#building) for Rust toolchain and system-dependency
requirements (FitSmith pulls in Slint, which needs `fontconfig` on Linux).

## Packaging

FitSmith can be packaged per OS. macOS and Linux use
[`cargo-bundle`](https://github.com/burtonageo/cargo-bundle) (install once with
`cargo install cargo-bundle`); Windows uses [NSIS](https://nsis.sourceforge.io/), an
open-source installer system with permissive licenses. The scripts drive the packaging — there
is no CI, since Slint's per-OS windowing/font dependencies mean each package has to be built
on that OS:

```shell
./build/package-unix.sh        # macOS -> .dmg, Linux -> .deb
./build/package-rpm.sh         # Linux -> .rpm
./build/package-windows.ps1    # Windows -> .exe installer (needs NSIS / makensis on PATH)
```

On Linux, building an `.rpm` additionally needs `rpmbuild` and `cargo-generate-rpm`; see
`build/package-rpm.sh` for the install hints.

The Windows installer includes `fitsmith.exe`, `fitz.exe`, and the Bayer runtime DLL, and
installs Start Menu shortcuts plus an uninstaller. Its output is
`target/release/bundle/FitSmith-<version>-windows-x64-setup.exe`; the macOS and Linux package
outputs land under `target/release/bundle/<osx|deb|rpm>/`. Packages are unsigned — no Apple
Developer ID or Windows code-signing certificate is used — so first launch shows a Gatekeeper
warning on macOS (right-click the app -> Open) and a SmartScreen warning on Windows (More
info -> Run anyway).

The macOS bundle registers FitSmith as an "Open With" handler for `.fit`/`.fits`/`.fit.fz`/
`.fits.fz`.

Because FitSmith links Slint under the GPLv3 (see below), any packaged binary you distribute
carries that obligation — see [Slint and licensing](#slint-and-licensing).

## Slint and licensing

FitSmith's user interface is built with [Slint](https://slint.dev/). Slint is available under
several licenses; FitSmith uses it under the **GNU General Public License, version 3 (GPLv3)**.
Because of this, distributing FitSmith binaries is subject to the terms of the GPLv3. The rest
of the `fitz` project (the `libfitz` library and the `fitz` CLI) remains under the MIT
license — see [LICENSE](../LICENSE).
