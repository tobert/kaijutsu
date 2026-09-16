//! Command result projections shared by runtime consumers and transports.

/// Preserve the output tree and structured data. Existing rich JSON wins;
/// otherwise the data sideband supplies it. Kaish's output limiter can clear
/// the tree while retaining this sideband.
pub fn block_output_data(
    result: &kaish_kernel::interpreter::ExecResult,
) -> Option<kaijutsu_types::OutputData> {
    let output = result.output().cloned();
    let needs_rich_json = output.as_ref().is_none_or(|od| od.rich_json.is_none());
    if needs_rich_json && let Some(v) = result.data.as_ref() {
        let rich_json = kaish_kernel::interpreter::value_to_json(v);
        return Some(output.unwrap_or_default().with_rich_json(rich_json));
    }
    output
}

#[cfg(test)]
mod block_output_data_tests {
    use super::block_output_data;
    use kaish_kernel::ast::Value;
    use kaish_kernel::interpreter::ExecResult;

    #[test]
    fn prefers_real_output_data_when_present() {
        let od = kaijutsu_types::OutputData::new().with_rich_json(serde_json::json!("x"));
        let result = ExecResult::with_output(od.clone());

        let bridged = block_output_data(&result).expect("expected Some");
        assert_eq!(
            bridged.rich_json, od.rich_json,
            "a builtin that set real .output must be used verbatim"
        );
    }

    #[test]
    fn falls_back_to_data_sideband_as_rich_json() {
        let data = Value::Json(serde_json::json!(["bass", "bassline"]));
        let result = ExecResult::success_with_data("bass\nbassline", data);
        assert!(
            result.output().is_none(),
            "test premise: no real .output, only .data"
        );

        let bridged = block_output_data(&result).expect("expected Some");
        assert_eq!(
            bridged.rich_json,
            Some(serde_json::json!(["bass", "bassline"])),
            "rich_json must equal the .data sideband, JSON-converted"
        );
    }

    #[test]
    fn neither_output_nor_data_yields_none() {
        let result = ExecResult::success("plain text, no structure");
        assert!(block_output_data(&result).is_none());
    }

    #[test]
    fn merges_data_sideband_onto_a_real_output_tree() {
        // A builtin can set BOTH a real node-tree `.output` (the
        // app-renderable shape) AND an independent `.data` sideband. The
        // resulting OutputData must carry both — the tree in `root`, `.data`
        // back-filled into `rich_json` — not one clobbering the other.
        let tree = kaijutsu_types::OutputData::nodes(vec![kaijutsu_types::OutputNode::new("row")]);
        assert!(
            tree.rich_json.is_none(),
            "test premise: the real tree carries no rich_json of its own"
        );
        let mut result = ExecResult::with_output(tree.clone());
        result.data = Some(Value::Json(serde_json::json!({"k": "v"})));

        let bridged = block_output_data(&result).expect("expected Some");
        assert_eq!(
            bridged.root, tree.root,
            "the real node tree must survive untouched"
        );
        assert_eq!(
            bridged.rich_json,
            Some(serde_json::json!({"k": "v"})),
            "rich_json must be back-filled from .data since .output didn't set its own"
        );
    }
}

/// Render a hook replacement's text content for a block consumer.
pub fn shell_hook_result_text(result: &crate::mcp::KernelToolResult) -> String {
    result
        .content
        .iter()
        .map(|c| match c {
            crate::mcp::ToolContent::Text(s) => s.clone(),
            crate::mcp::ToolContent::Json(value) => value.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Supply PostCall hooks with stdout, stderr, the real exit, and spill state.
pub fn exec_result_to_hook_tool_result(
    result: &kaish_kernel::interpreter::ExecResult,
) -> crate::mcp::KernelToolResult {
    let real_code = result.original_code.unwrap_or(result.code);
    let is_error = real_code != 0;
    let stdout = result.text_out().into_owned();
    let mut body = stdout.clone();
    if !result.err.is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&result.err);
    }
    crate::mcp::KernelToolResult {
        is_error,
        content: vec![crate::mcp::ToolContent::Text(body)],
        structured: Some(serde_json::json!({
            "stdout": stdout,
            "stderr": result.err,
            "exit_code": real_code,
            "did_spill": result.did_spill,
        })),
    }
}

#[cfg(test)]
mod exec_result_to_hook_tool_result_tests {
    use super::exec_result_to_hook_tool_result;
    use crate::mcp::ToolContent;
    use kaish_kernel::interpreter::ExecResult;

    /// PostCall receives the executed command's stdout and exit code.
    #[test]
    fn carries_the_real_exit_code_and_stdout() {
        let mut result = ExecResult::success("unmistakable-real-stdout");
        result.code = 0;
        let hook_result = exec_result_to_hook_tool_result(&result);
        assert!(!hook_result.is_error);
        let text = match &hook_result.content[0] {
            ToolContent::Text(t) => t.clone(),
            other => panic!("expected text content, got {other:?}"),
        };
        assert!(
            text.contains("unmistakable-real-stdout"),
            "hook body must see the real stdout: {text}",
        );
        assert_eq!(
            hook_result
                .structured
                .as_ref()
                .and_then(|s| s.get("exit_code"))
                .and_then(|v| v.as_i64()),
            Some(0),
        );
    }

    /// A spilled result remaps `code` to 3 (kaish's output-limit contract);
    /// the hook must be judged by the REAL exit (`original_code`), the same
    /// rule `shell_result_to_envelope` applies ("truncation is not failure").
    /// Falsification: read `result.code` instead of `real_code` here and
    /// this test goes red (`is_error` flips to `true`, `exit_code` reads 3).
    #[test]
    fn judges_a_spilled_result_by_its_real_original_exit_code() {
        let mut result = ExecResult::success("partial output");
        result.code = 3;
        result.did_spill = true;
        result.original_code = Some(0);
        let hook_result = exec_result_to_hook_tool_result(&result);
        assert!(
            !hook_result.is_error,
            "a spilled-but-successful command must not read back as an error to the hook",
        );
        assert_eq!(
            hook_result
                .structured
                .as_ref()
                .and_then(|s| s.get("exit_code"))
                .and_then(|v| v.as_i64()),
            Some(0),
        );
    }
}

/// Project a kaish result into the public shell envelope. Truncation retains
/// the command's original exit code; the spill flag reports the output limit.
pub fn shell_result_to_envelope(
    result: kaish_kernel::interpreter::ExecResult,
    elapsed_ms: u64,
) -> kaijutsu_types::shell_envelope::ShellEnvelope {
    use kaijutsu_types::shell_envelope::ShellEnvelope;

    let exit_code = result.original_code.unwrap_or(result.code);
    let mut env = ShellEnvelope::new(ShellEnvelope::status_for_exit(exit_code));
    env.stdout = result.text_out().into_owned();
    env.stderr = result.err.clone();
    env.exit_code = Some(exit_code);
    env.did_spill = Some(result.did_spill);
    env.elapsed_ms = Some(elapsed_ms);
    env.content_type = Some(result.content_type.clone().unwrap_or_else(|| "text/plain".into()));
    env.ephemeral = Some(result.baggage.get("kaijutsu.ephemeral").is_some_and(|value| value == "true"));
    // Keep structured payloads separate from rendered stdout.
    env.data = result
        .data
        .as_ref()
        .map(kaish_kernel::interpreter::value_to_json);
    // Confirmation hints travel separately from the command's structured data.
    env.latch = crate::runtime::kj_builtin::latch_from_result(&result).map(|l| {
        serde_json::json!({
            "command": l.command,
            "target": l.target,
            "hint": l.hint,
        })
    });
    env
}
