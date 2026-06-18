# Gemini CLI hook payload -> kaijutsu HookEvent field mapping.
#
# Single source of truth for the field map (gemini.sh wires the event-name
# case statement and pipes through `jq -f` here). Note Gemini nests tool
# output under `.tool_response` (llmContent / error), unlike Claude's flat
# `.tool_output` — that difference is intentional, not a copy of claude.jq.
#
# Invoke with the mapped kaijutsu event name: jq --arg event tool.after -f …
{
    event: $event,
    source: "gemini-cli",
    session_id: .session_id,
    cwd: .cwd,
    transcript_path: .transcript_path,
    prompt: .prompt,
    response: .response,
    tool: (if .tool_name then {
        name: .tool_name,
        input: .tool_input,
        output: (.tool_response.llmContent // .tool_response // null),
        error: (.tool_response.error // null)
    } else null end),
    principal_id: .agent_id,
    agent_type: .agent_type,
    reason: .reason
}
