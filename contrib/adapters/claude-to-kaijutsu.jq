# Claude Code hook payload -> kaijutsu HookEvent field mapping.
#
# Single source of truth for the field map (claude.sh wires the event-name
# case statement and pipes through `jq -f` here). Kept testable on its own:
# crates/kaijutsu-mcp/tests/adapter_mapping.rs round-trips real Claude
# payloads through this filter into kaijutsu_mcp::hook_types::HookEvent and
# asserts no field is silently dropped.
#
# Invoke with the mapped kaijutsu event name: jq --arg event tool.after -f …
{
    event: $event,
    source: "claude-code",
    session_id: .session_id,
    cwd: .cwd,
    model: .model,
    transcript_path: .transcript_path,
    prompt: .prompt,
    response: .response,
    tool: (if .tool_name then {
        name: .tool_name,
        input: .tool_input,
        output: (.tool_output // null),
        error: (.error // null)
    } else null end),
    principal_id: .agent_id,
    agent_type: .agent_type,
    reason: .reason
}
