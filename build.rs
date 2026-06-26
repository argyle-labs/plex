//! Generate the typed Plex client from the vendored upstream spec.
//!
//! The vendored spec (`specs/plex.openapi.json`, OpenAPI 3.1.1, MIT) is a
//! 3.1 document. `generate_all` detects the 3.1 version and runs the toolkit's
//! 3.1→3.0 lowering pass (`openapi::lower_31`) before pruning to the handful of
//! endpoints this plugin drives plus their transitive schema closure, then
//! runs progenitor. Pruning lives in `plugin_toolkit_build` per
//! [[feedback-plugin-toolkit-is-the-gateway]]; never hand-patch a spec here.
//!
//! The emitted module is named `plex` (the flavor, basename minus the
//! `.openapi.json` suffix) and `include!`d from `src/lib.rs`.

fn main() {
    let specs = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("specs");

    plugin_toolkit_build::openapi::generate_selected(
        specs,
        "plex",
        &[(
            "plex",
            &[
                "/",
                "/identity",
                "/library/sections/all",
                "/status/sessions",
            ],
        )],
    )
    .expect("plex openapi codegen");
}
