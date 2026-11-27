//! Build script for the optional Slint UI.
//!
//! The script is inert unless `native-ui` or `player-overlay` is enabled. It
//! never reads the network, never writes outside `OUT_DIR`, and only recompiles
//! when one of the declarative UI files changes. The two features compile
//! separate roots: the launcher (`ui/app.slint`, included with
//! `slint::include_modules!`) and the native-player control bar
//! (`ui/player_overlay.slint`, included from the child binary by path).

use std::path::Path;

/// Declarative UI sources, relative to the package root.
const UI_SOURCES: [&str; 9] = [
    "ui/app.slint",
    "ui/cast.slint",
    "ui/home.slint",
    "ui/jellyfin.slint",
    "ui/library.slint",
    "ui/pair.slint",
    "ui/player.slint",
    "ui/player_overlay.slint",
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

    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let out_dir = std::env::var_os("OUT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    if std::env::var_os("CARGO_FEATURE_NATIVE_UI").is_some() {
        let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
        slint_build::compile_with_config("ui/app.slint", config)
            .expect("failed to compile ui/app.slint");
    }

    if std::env::var_os("CARGO_FEATURE_PLAYER_OVERLAY").is_some() {
        // A dedicated output path keeps `SLINT_INCLUDE_GENERATED` pointing at
        // the launcher module when both features are enabled. The child binary
        // includes `player_overlay.rs` explicitly. `compile_with_output_path`
        // takes absolute paths and prints no cargo directives of its own.
        let input = manifest_dir.join("ui/player_overlay.slint");
        let output = out_dir.join("player_overlay.rs");
        slint_build::compile_with_output_path(
            &input,
            &output,
            slint_build::CompilerConfiguration::new(),
        )
        .expect("failed to compile ui/player_overlay.slint");
    }
}
