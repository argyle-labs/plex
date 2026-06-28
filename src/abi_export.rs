// The tool surface crosses this FFI boundary as opaque JSON â the designated
// JSON dispatch seam, identical to orca's `plugin-loader` and
// `dispatch::ErasedTool::run_json`. The payload type is aliased (`sj`) at this
// one seam, exactly as the loader aliases it, and the workspace
// disallowed-types lint is suppressed for this file only.
#![allow(clippy::disallowed_types)]

//! ABI-stable cdylib export.
//!
//! Builds and exports the single [`PluginModRef`] root module orca's
//! `plugin-loader` `dlopen`s. The six accessor fns carry the version header the
//! loader reads before invoking anything; `manifest`/`invoke` wrap this crate's
//! own statically-linked tool inventory (`server_info` / `libraries` /
//! `transcode_health` + the lifecycle tools + the `endpoint_resource!` CRUD)
//! through the toolkit's re-exported dispatch surface.
//!
//! Only the entrypoint + metadata cross as `StableAbi` types; the tool surface
//! itself crosses as JSON (manifest array + invoke args/result strings),
//! exactly as the toolkit `abi` contract specifies.

use std::sync::Arc;
use std::sync::OnceLock;

// The `#[export_root_module]` attribute expands to bare `::abi_stable` paths in
// this crate's root, so `abi_stable` must be a direct dependency â it is a
// genuinely-external (non-orca) crate, exactly like the progenitor/serde deps
// the generated client carries. Pinned to the toolkit's version so the layout
// hash the loader checks matches.
use abi_stable::export_root_module;
use abi_stable::prefix_type::PrefixTypeTrait;
use abi_stable::std_types::{RErr, ROk, RResult, RStr, RString};
use plugin_toolkit::abi::{PluginMod, PluginModRef, ToolDef};
use plugin_toolkit::contract::config::{Config, Model, Ports};
use plugin_toolkit::contract::ToolCtx;
use plugin_toolkit::dispatch::{dispatch, tool_manifest_json};
// The JSON dispatch payload type, named once here at the designated opaque seam.
use plugin_toolkit::serde_json as sj;
use plugin_toolkit::tokio::runtime::{Builder, Runtime};

extern "C" fn plugin_semver() -> RString {
    RString::from(env!("CARGO_PKG_VERSION"))
}

extern "C" fn target_software() -> RString {
    RString::from("plex")
}

extern "C" fn target_compat() -> RString {
    RString::from("1.40-1.41")
}

extern "C" fn orca_compat() -> RString {
    RString::from(">=0.0.8, <0.1.0")
}

/// Tool-name prefix this plugin owns. The cdylib statically links the toolkit's
/// domain crates (containers / notifications / â¦), each of which carries its
/// own `#[orca_tool]` inventory entries, so the raw `tool_manifest_json()` walk
/// returns those host-owned tools alongside the plugin's. The plugin exposes
/// only its own `plex.*` namespace across the ABI; the host already owns the
/// domain tools and would otherwise reject the manifest as colliding built-ins.
const TOOL_PREFIX: &str = "plex.";

/// The plugin's own tool surface: `tool_manifest_json()` filtered to the
/// `plex.*` namespace. Shared by `manifest()` (serialized back out) and
/// `invoke()` (admission check) so both agree on exactly which tools cross.
fn own_tools() -> Vec<ToolDef> {
    let all: Vec<ToolDef> = sj::from_str(&tool_manifest_json()).unwrap_or_default();
    all.into_iter()
        .filter(|d| d.name.starts_with(TOOL_PREFIX))
        .collect()
}

extern "C" fn manifest() -> RString {
    let defs = own_tools();
    RString::from(sj::to_string(&defs).unwrap_or_else(|_| "[]".to_string()))
}

/// Shared multi-thread runtime driving the async tool bodies behind the
/// synchronous FFI `invoke`. Built once on first call and kept for the process
/// lifetime so repeated invocations don't spin a fresh runtime each time.
fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build plugin tokio runtime")
    })
}

/// A minimal `ToolCtx` for in-cdylib dispatch. The tool surface this plugin
/// exposes (HTTP-only endpoint CRUD + diagnosis + process-exec lifecycle) needs
/// no host-injected services, so an empty service registry over a placeholder
/// config suffices; any tool reaching for a service errors cleanly rather than
/// panicking.
fn minimal_ctx() -> ToolCtx {
    let config = Config {
        anthropic_api_key: None,
        lmstudio_url: String::new(),
        ollama_url: String::new(),
        default_model: Model::LMStudio {
            id: String::new(),
            url: String::new(),
        },
        app_dir: std::env::temp_dir(),
        memory_root: std::env::temp_dir(),
        db_path: std::env::temp_dir().join("orca-plugin.db"),
        ports: Ports::default(),
    };
    ToolCtx::new(Arc::new(config))
}

extern "C" fn invoke(name: RStr<'_>, args_json: RStr<'_>) -> RResult<RString, RString> {
    if !name.as_str().starts_with(TOOL_PREFIX) {
        return RErr(RString::from(format!(
            "tool '{}' is not in this plugin's '{TOOL_PREFIX}' namespace",
            name.as_str()
        )));
    }
    let args: sj::Value = match sj::from_str(args_json.as_str()) {
        Ok(v) => v,
        Err(e) => return RErr(RString::from(format!("invalid args JSON: {e}"))),
    };
    let ctx = minimal_ctx();
    let result = runtime().block_on(dispatch(name.as_str(), args, &ctx));
    match result {
        Ok(value) => match sj::to_string(&value) {
            Ok(s) => ROk(RString::from(s)),
            Err(e) => RErr(RString::from(format!("failed to encode result: {e}"))),
        },
        Err(e) => RErr(RString::from(format!("{e:#}"))),
    }
}

/// Domain backends this plugin contributes. Pure tool-surface plugin (no
/// storage/etc. backend), so it contributes none — an empty array, identical to
/// what the toolkit per-field default would synthesize for a plugin that predates
/// the `backends` ABI field.
extern "C" fn backends() -> RString {
    RString::from("[]")
}

/// Declared SQL tables: none yet (this plugin owns no plugin-scoped tables).
/// Empty declaration matches what orca synthesizes for a plugin predating the
/// field; a stateful plugin would return a real SchemaDecl here.
extern "C" fn schemas() -> RString {
    RString::from(r#"{"namespace":"","tables":[]}"#)
}

#[export_root_module]
fn export() -> PluginModRef {
    PluginMod {
        plugin_semver,
        target_software,
        target_compat,
        orca_compat,
        manifest,
        invoke,
        backends,
        schemas,
    }
    .leak_into_prefix()
}
