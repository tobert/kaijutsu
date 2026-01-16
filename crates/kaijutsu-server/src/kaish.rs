//! Kaish runtime integration for kaijutsu-server.
//!
//! This module embeds kaish-kernel to provide the execution layer for kernels.
//! kaijutsu adds collaboration on top (lease, consent, checkpoint, messaging).

use anyhow::Result;

use kaish_kernel::ast::{Arg, Expr, Pipeline, Stmt, Value};
use kaish_kernel::interpreter::{ExecResult, Scope};
use kaish_kernel::parser::parse;

/// Kaish execution runtime.
///
/// Wraps the kaish interpreter to provide command execution for a kaijutsu kernel.
/// Each KaishRuntime maintains its own variable scope and execution state.
pub struct KaishRuntime {
    scope: Scope,
}

impl KaishRuntime {
    /// Create a new kaish runtime.
    pub fn new() -> Self {
        Self {
            scope: Scope::new(),
        }
    }

    /// Execute kaish code and return the result.
    ///
    /// This is the main entry point for kaijutsu's `Kernel.execute()` RPC method.
    pub fn execute(&mut self, code: &str) -> Result<ExecResult> {
        let trimmed = code.trim();

        // Skip empty input
        if trimmed.is_empty() {
            return Ok(ExecResult::success(""));
        }

        // Parse the input
        let program = match parse(trimmed) {
            Ok(prog) => prog,
            Err(errors) => {
                let mut msg = String::from("Parse error:\n");
                for err in errors {
                    msg.push_str(&format!("  {err}\n"));
                }
                return Ok(ExecResult::failure(1, msg));
            }
        };

        // Execute each statement, collecting results
        let mut last_result = ExecResult::success("");
        for stmt in program.statements {
            last_result = self.execute_stmt(&stmt)?;
        }

        Ok(last_result)
    }

    /// Execute a single statement.
    fn execute_stmt(&mut self, stmt: &Stmt) -> Result<ExecResult> {
        match stmt {
            Stmt::Assignment(assign) => {
                let value = self.eval_expr(&assign.value)?;
                self.scope.set(&assign.name, value.clone());
                Ok(ExecResult::success(format!(
                    "{} = {}",
                    assign.name,
                    format_value(&value)
                )))
            }
            Stmt::Command(cmd) => {
                let result = self.execute_command(&cmd.name, &cmd.args)?;
                self.scope.set_last_result(result.clone());
                Ok(result)
            }
            Stmt::Pipeline(pipeline) => {
                let result = self.execute_pipeline(pipeline)?;
                self.scope.set_last_result(result.clone());
                Ok(result)
            }
            Stmt::If(if_stmt) => {
                let cond_value = self.eval_expr(&if_stmt.condition)?;
                let branch = if is_truthy(&cond_value) {
                    &if_stmt.then_branch
                } else {
                    if_stmt
                        .else_branch
                        .as_ref()
                        .map(|v| v.as_slice())
                        .unwrap_or(&[])
                };

                let mut last_result = ExecResult::success("");
                for stmt in branch {
                    last_result = self.execute_stmt(stmt)?;
                }
                Ok(last_result)
            }
            Stmt::For(for_loop) => {
                let iterable = self.eval_expr(&for_loop.iterable)?;
                let items = match iterable {
                    Value::Array(items) => items,
                    _ => return Ok(ExecResult::failure(1, "for loop requires an array")),
                };

                self.scope.push_frame();
                let mut last_result = ExecResult::success("");

                for item in items {
                    if let Expr::Literal(value) = item {
                        self.scope.set(&for_loop.variable, value);
                        for stmt in &for_loop.body {
                            last_result = self.execute_stmt(stmt)?;
                        }
                    }
                }

                self.scope.pop_frame();
                Ok(last_result)
            }
            Stmt::ToolDef(tool) => Ok(ExecResult::success(format!("Defined tool: {}", tool.name))),
            Stmt::Empty => Ok(ExecResult::success("")),
        }
    }

    /// Execute a command.
    fn execute_command(&mut self, name: &str, args: &[Arg]) -> Result<ExecResult> {
        // Evaluate arguments
        let mut evaluated_args = Vec::new();
        for arg in args {
            match arg {
                Arg::Positional(expr) => {
                    let value = self.eval_expr(expr)?;
                    evaluated_args.push(format_value(&value));
                }
                Arg::Named { key, value } => {
                    let val = self.eval_expr(value)?;
                    evaluated_args.push(format!("{}={}", key, format_value(&val)));
                }
                Arg::ShortFlag(flag) => {
                    evaluated_args.push(format!("-{}", flag));
                }
                Arg::LongFlag(flag) => {
                    evaluated_args.push(format!("--{}", flag));
                }
            }
        }

        // Handle built-in commands
        match name {
            "echo" => {
                let output: Vec<String> = args
                    .iter()
                    .map(|arg| match arg {
                        Arg::Positional(expr) => {
                            if let Ok(value) = self.eval_expr(expr) {
                                format_value_unquoted(&value)
                            } else {
                                "<error>".to_string()
                            }
                        }
                        Arg::Named { key, value } => {
                            if let Ok(val) = self.eval_expr(value) {
                                format!("{}={}", key, format_value_unquoted(&val))
                            } else {
                                format!("{}=<error>", key)
                            }
                        }
                        Arg::ShortFlag(flag) => format!("-{}", flag),
                        Arg::LongFlag(flag) => format!("--{}", flag),
                    })
                    .collect();
                Ok(ExecResult::success(output.join(" ")))
            }
            "true" => Ok(ExecResult::success("")),
            "false" => Ok(ExecResult::failure(1, "")),
            _ => {
                // Stub: show what would be executed
                let cmd_line = if evaluated_args.is_empty() {
                    name.to_string()
                } else {
                    format!("{} {}", name, evaluated_args.join(" "))
                };
                Ok(ExecResult::success(format!("[stub] {}", cmd_line)))
            }
        }
    }

    /// Execute a pipeline.
    fn execute_pipeline(&mut self, pipeline: &Pipeline) -> Result<ExecResult> {
        if pipeline.commands.len() == 1 {
            let cmd = &pipeline.commands[0];
            let mut result = self.execute_command(&cmd.name, &cmd.args)?;
            if pipeline.background {
                result = ExecResult::success(format!("[bg] {}", result.out));
            }
            return Ok(result);
        }

        // Multi-command pipeline: stub for now
        let cmd_names: Vec<_> = pipeline.commands.iter().map(|c| c.name.as_str()).collect();
        let pipeline_str = cmd_names.join(" | ");

        if pipeline.background {
            Ok(ExecResult::success(format!("[stub] {} &", pipeline_str)))
        } else {
            Ok(ExecResult::success(format!(
                "[stub pipeline] {}",
                pipeline_str
            )))
        }
    }

    /// Evaluate an expression.
    fn eval_expr(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Literal(value) => self.eval_literal(value),
            Expr::VarRef(path) => self
                .scope
                .resolve_path(path)
                .ok_or_else(|| anyhow::anyhow!("undefined variable")),
            Expr::Interpolated(parts) => {
                use kaish_kernel::ast::StringPart;
                let mut result = String::new();
                for part in parts {
                    match part {
                        StringPart::Literal(s) => result.push_str(s),
                        StringPart::Var(path) => {
                            let value = self
                                .scope
                                .resolve_path(path)
                                .ok_or_else(|| anyhow::anyhow!("undefined variable"))?;
                            result.push_str(&format_value_unquoted(&value));
                        }
                    }
                }
                Ok(Value::String(result))
            }
            Expr::BinaryOp { left, op, right } => {
                use kaish_kernel::ast::BinaryOp;
                match op {
                    BinaryOp::And => {
                        let left_val = self.eval_expr(left)?;
                        if !is_truthy(&left_val) {
                            return Ok(left_val);
                        }
                        self.eval_expr(right)
                    }
                    BinaryOp::Or => {
                        let left_val = self.eval_expr(left)?;
                        if is_truthy(&left_val) {
                            return Ok(left_val);
                        }
                        self.eval_expr(right)
                    }
                    BinaryOp::Eq => {
                        let l = self.eval_expr(left)?;
                        let r = self.eval_expr(right)?;
                        Ok(Value::Bool(values_equal(&l, &r)))
                    }
                    BinaryOp::NotEq => {
                        let l = self.eval_expr(left)?;
                        let r = self.eval_expr(right)?;
                        Ok(Value::Bool(!values_equal(&l, &r)))
                    }
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                        let l = self.eval_expr(left)?;
                        let r = self.eval_expr(right)?;
                        let ord = compare_values(&l, &r)?;
                        let result = match op {
                            BinaryOp::Lt => ord.is_lt(),
                            BinaryOp::Gt => ord.is_gt(),
                            BinaryOp::LtEq => ord.is_le(),
                            BinaryOp::GtEq => ord.is_ge(),
                            _ => unreachable!(),
                        };
                        Ok(Value::Bool(result))
                    }
                }
            }
            Expr::CommandSubst(pipeline) => {
                let result = self.execute_pipeline(pipeline)?;
                self.scope.set_last_result(result.clone());
                Ok(result_to_value(&result))
            }
        }
    }

    fn eval_literal(&mut self, value: &Value) -> Result<Value> {
        match value {
            Value::Array(items) => {
                let evaluated: Result<Vec<_>> = items
                    .iter()
                    .map(|expr| self.eval_expr(expr).map(|v| Expr::Literal(v)))
                    .collect();
                Ok(Value::Array(evaluated?))
            }
            Value::Object(fields) => {
                let evaluated: Result<Vec<_>> = fields
                    .iter()
                    .map(|(k, expr)| self.eval_expr(expr).map(|v| (k.clone(), Expr::Literal(v))))
                    .collect();
                Ok(Value::Object(evaluated?))
            }
            _ => Ok(value.clone()),
        }
    }

    /// Get the last execution result.
    pub fn last_result(&self) -> &ExecResult {
        self.scope.last_result()
    }

    /// Get a variable value.
    pub fn get_var(&self, name: &str) -> Option<&Value> {
        self.scope.get(name)
    }

    /// Set a variable value.
    pub fn set_var(&mut self, name: &str, value: Value) {
        self.scope.set(name, value);
    }

    /// List all variable names.
    pub fn list_vars(&self) -> Vec<&str> {
        self.scope.all_names()
    }
}

impl Default for KaishRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Format a Value for display (with quotes on strings).
fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::String(s) => format!("\"{}\"", s),
        Value::Array(items) => {
            let formatted: Vec<String> = items
                .iter()
                .filter_map(|e| {
                    if let Expr::Literal(v) = e {
                        Some(format_value(v))
                    } else {
                        Some("<expr>".to_string())
                    }
                })
                .collect();
            format!("[{}]", formatted.join(", "))
        }
        Value::Object(fields) => {
            let formatted: Vec<String> = fields
                .iter()
                .map(|(k, e)| {
                    let v = if let Expr::Literal(v) = e {
                        format_value(v)
                    } else {
                        "<expr>".to_string()
                    };
                    format!("\"{}\": {}", k, v)
                })
                .collect();
            format!("{{{}}}", formatted.join(", "))
        }
    }
}

/// Format a Value for display (without quotes on strings).
fn format_value_unquoted(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        _ => format_value(value),
    }
}

/// Check if a value is truthy.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        Value::Float(f) => *f != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(arr) => !arr.is_empty(),
        Value::Object(_) => true,
    }
}

/// Check if two values are equal.
fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => (a - b).abs() < f64::EPSILON,
        (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => {
            (*a as f64 - b).abs() < f64::EPSILON
        }
        (Value::String(a), Value::String(b)) => a == b,
        _ => false,
    }
}

/// Compare two values for ordering.
fn compare_values(left: &Value, right: &Value) -> Result<std::cmp::Ordering> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => a
            .partial_cmp(b)
            .ok_or_else(|| anyhow::anyhow!("NaN comparison")),
        (Value::Int(a), Value::Float(b)) => (*a as f64)
            .partial_cmp(b)
            .ok_or_else(|| anyhow::anyhow!("NaN comparison")),
        (Value::Float(a), Value::Int(b)) => a
            .partial_cmp(&(*b as f64))
            .ok_or_else(|| anyhow::anyhow!("NaN comparison")),
        (Value::String(a), Value::String(b)) => Ok(a.cmp(b)),
        _ => Err(anyhow::anyhow!("cannot compare these types")),
    }
}

/// Convert an ExecResult to a Value.
fn result_to_value(result: &ExecResult) -> Value {
    let mut fields = vec![
        ("code".into(), Expr::Literal(Value::Int(result.code))),
        ("ok".into(), Expr::Literal(Value::Bool(result.ok()))),
        (
            "out".into(),
            Expr::Literal(Value::String(result.out.clone())),
        ),
        (
            "err".into(),
            Expr::Literal(Value::String(result.err.clone())),
        ),
    ];
    if let Some(data) = &result.data {
        fields.push(("data".into(), Expr::Literal(data.clone())));
    }
    Value::Object(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_echo() {
        let mut rt = KaishRuntime::new();
        let result = rt.execute("echo hello world").unwrap();
        assert!(result.ok());
        assert_eq!(result.out, "hello world");
    }

    #[test]
    fn execute_assignment() {
        let mut rt = KaishRuntime::new();
        let result = rt.execute("set X = 42").unwrap();
        assert!(result.ok());
        assert_eq!(rt.get_var("X"), Some(&Value::Int(42)));
    }

    #[test]
    fn execute_interpolation() {
        let mut rt = KaishRuntime::new();
        rt.execute("set NAME = \"World\"").unwrap();
        let result = rt.execute("echo \"Hello ${NAME}\"").unwrap();
        assert!(result.ok());
        assert_eq!(result.out, "Hello World");
    }

    #[test]
    fn execute_parse_error() {
        let mut rt = KaishRuntime::new();
        let result = rt.execute("set X =").unwrap();
        assert!(!result.ok());
        assert!(result.err.contains("Parse error"));
    }
}
