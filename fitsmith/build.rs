//! Compile the Slint UI. The Fluent widget style is baked in here (rather than
//! left to the `SLINT_STYLE` env var) so the build is self-describing.

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".to_string());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compilation failed");
}
