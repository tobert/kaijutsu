# Codex CLI hook payload -> kaijutsu HookEvent field mapping.
#
# Codex and Claude use the same JSONL hook shape for the common lifecycle
# events, but Codex has a few source-specific fields: `last_assistant_message`
# is the authoritative Stop response, `trigger` describes compaction, and
# tool results arrive in `tool_response`. Keep this map independent from the
# shell transport so it can be round-tripped in adapter_mapping.rs.
#
# Invoke with the normalized kaijutsu event name:
#   jq -c --arg event tool.after -f codex-to-kaijutsu.jq
{
    event: $event,
    source: "codex",
    session_id: .session_id,
    timestamp: .timestamp,
    cwd: .cwd,
    model: .model,
    transcript_path: .transcript_path,
    prompt: .prompt,
    # Codex calls this field `last_assistant_message` on Stop. Keep the
    # generic response fallback for synthetic/older hook payloads.
    response: (.last_assistant_message // .response),
    tool: (if .tool_name then {
        name: .tool_name,
        input: .tool_input,
        output: ((.tool_response // .tool_output)
                 | if . == null then null
                   elif type == "string" then .
                   else tojson end),
        error: (.error // null),
        duration_ms: (.duration_ms // null)
    } else null end),
    file: (if .file then .file else null end),
    principal_id: (.agent_id // .subagent_id // .principal_id),
    agent_type: (.agent_type // .subagent_type),
    reason: .reason,
    trigger: .trigger
}
