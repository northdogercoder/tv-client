//! Build script for the optional Slint UI.
//!
//! The script is inert unless the `native-ui` feature is enabled. It never
//! reads the network, never writes outside `OUT_DIR`, and only recompiles when
//! one of the declarative UI files changes.

use std::path::Path;

/// Declarative UI sources, relative to the package root.
const UI_SOURCES: [&str; 8] = [
    "ui/app.slint",
    "ui/cast.slint",
    "ui/home.slint",
    "ui/jellyfin.slint",
    "ui/library.slint",
    "ui/pair.slint",
    "ui/player.slint",
    "ui/settings.slint",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for source in UI_SOURCES {
        assert!(
            Path::new(source).is_file(),
            "declarative UI source is missing: {source}"
        );
        println!("cargo:rerun-if-changed={source}");
    }

    if std::env::var_os("CARGO_FEATURE_NATIVE_UI").is_none() {
        return;
    }

    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("failed to compile ui/app.slint");
}
