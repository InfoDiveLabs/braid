fn main() {
    println!("cargo:rerun-if-changed=ui");

    // Element introspection (get_element_tree, find_elements_by_id, and the
    // i-slint-backend-testing ElementHandle API) requires debug info emitted at
    // *compile* time. Tie it to `devtools` so release builds carry none of it.
    let debug_info = std::env::var_os("CARGO_FEATURE_DEVTOOLS").is_some();

    // The UI imports platform tokens as `@platform`; the file behind that name
    // is chosen here. A library path rather than a second entry point, so
    // there is exactly one copy of the layout and only the values that
    // genuinely differ per target are swapped: and the ones for other targets
    // are never compiled in at all.
    // `DL_UI_PLATFORM` overrides the choice, so the other platform's look can
    // be built and captured from here. Without it there is no way to see what
    // a change does to Windows and Linux short of booting them, and the two
    // token files drift.
    println!("cargo:rerun-if-env-changed=DL_UI_PLATFORM");
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let choice = std::env::var("DL_UI_PLATFORM").unwrap_or(target);
    let tokens = match choice.as_str() {
        "macos" => "ui/platform/macos.slint",
        _ => "ui/platform/generic.slint",
    };
    println!("cargo:rerun-if-changed={tokens}");

    let config =
        slint_build::CompilerConfiguration::new().with_debug_info(debug_info).with_library_paths(
            [("platform".to_string(), std::path::PathBuf::from(tokens))].into_iter().collect(),
        );
    slint_build::compile_with_config("ui/app.slint", config)
        .expect("failed to compile ui/app.slint");
}
