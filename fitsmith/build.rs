//! Compile the Slint UI. The Fluent widget style is baked in here (rather than
//! left to the `SLINT_STYLE` env var) so the build is self-describing.
//!
//! On Windows targets, also embed the application icon into the executable.

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".to_string());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compilation failed");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/fitsmith.ico");
        winresource::WindowsResource::new()
            .set_icon("assets/fitsmith.ico")
            .compile()
            .expect("embedding the Windows icon resource failed");
    }
}
