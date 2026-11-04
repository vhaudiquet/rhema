//! The MCP CLI as a standalone binary — the test harness's exec
//! target and a debugging aid. The agent shells' stubs exec the
//! rhema binary itself (`RHEMA_MCP_BIN`), which routes to the same
//! `rhema_mcp::cli::run`; this target exists so provision's exec
//! chain can be tested end-to-end without building the frontend.

fn main() {
    let argv: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    std::process::exit(rhema_mcp::cli::run(&argv));
}
