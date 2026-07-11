//! Dynamic (subprocess) entrypoint for the plex plugin.
//!
//! The toolkit's `serve_tool_plugin!` emits `fn main`, serving this plugin over the orca
//! socket. Dynamic replacement for the retired cdylib export — the plugin is a
//! `[[bin]]`, owns no runtime, and reaches orca only through the socket.
plugin_toolkit::serve_tool_plugin! {
    name: "plex",
    target_compat: "1.40-1.41",
}
