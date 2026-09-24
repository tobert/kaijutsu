//! The read-only `git` tool: registered only in `ShellPolicy::ReadOnly`
//! shells (`runtime::context_shell`'s `configure_tools` closure).
//!
//! `kaish_tools_git::GitConfig::read_only()` is the only constructor — read
//! profile, every implemented verb, tool name `git` (deliberately shadowing
//! external `git`, since kaish resolves builtins before `PATH`), default
//! `Limits`. Read-only-ness is a property of this crate's construction, not
//! a flag this module toggles: see `docs/embedding-git.md` in
//! `~/src/kaish-extras`, "The read-only story: five layers, stated
//! precisely". The writable shell never registers this tool; it keeps host
//! `git` reachable through `ExternalExec::Allow` instead.

/// Build the read-only `git` tool a read-only context shell registers.
///
/// `tool()` only fails for a config that could never work (an empty verb set
/// or an unusable tool name) — neither is reachable from a fixed
/// `GitConfig::read_only()` call with no further narrowing, so a failure here
/// is a programming error, not a runtime condition: panic loudly rather than
/// let a read-only shell materialize without `git`.
pub fn git_tool() -> kaish_tools_git::GitTool {
    kaish_tools_git::tool(kaish_tools_git::GitConfig::read_only())
        .expect("GitConfig::read_only() is always a valid config")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_tool_registers_under_the_name_git() {
        let mut registry = kaish_kernel::ToolRegistry::new();
        registry.register(git_tool());
        assert!(
            registry.contains("git"),
            "git_tool() must register under the name \"git\" — got: {:?}",
            registry.names()
        );
    }
}
