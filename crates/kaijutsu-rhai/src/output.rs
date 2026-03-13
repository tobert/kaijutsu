//! Output capture for Rhai scripts.
//!
//! `OutputCollector` captures `svg()` and `print()` output from Rhai scripts
//! using `Arc<Mutex<>>` instead of thread-locals, making it safe for concurrent use.

use rhai::{Engine, ImmutableString};
use std::sync::{Arc, Mutex};

/// Internal state for output capture.
#[derive(Debug, Default)]
struct OutputState {
    svg: Option<String>,
    stdout: String,
}

/// Captures output from Rhai scripts (`svg()` and `print()` calls).
///
/// Created by [`register_output_callbacks`] and shared with the engine.
/// Thread-safe via `Arc<Mutex<>>`.
#[derive(Debug, Clone)]
pub struct OutputCollector {
    inner: Arc<Mutex<OutputState>>,
}

impl OutputCollector {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(OutputState::default())),
        }
    }

    /// Take the captured SVG content, leaving None in its place.
    pub fn take_svg(&self) -> Option<String> {
        self.inner.lock().unwrap().svg.take()
    }

    /// Take the captured stdout, leaving an empty string.
    pub fn take_stdout(&self) -> String {
        std::mem::take(&mut self.inner.lock().unwrap().stdout)
    }

    /// Clear all captured output.
    pub fn clear(&self) {
        let mut state = self.inner.lock().unwrap();
        state.svg = None;
        state.stdout.clear();
    }
}

/// Register `svg()` and `print()`/`debug()` capture on a Rhai engine.
///
/// Returns an `OutputCollector` that receives the captured output.
/// The collector can be queried after script execution.
pub fn register_output_callbacks(engine: &mut Engine) -> OutputCollector {
    let collector = OutputCollector::new();

    // svg(content) — set SVG output
    let svg_collector = collector.inner.clone();
    engine.register_fn("svg", move |content: ImmutableString| {
        svg_collector.lock().unwrap().svg = Some(content.to_string());
    });

    // Override print to capture stdout
    let print_collector = collector.inner.clone();
    engine.on_print(move |s| {
        let mut state = print_collector.lock().unwrap();
        state.stdout.push_str(s);
        state.stdout.push('\n');
    });

    // Override debug to capture stdout with source info
    let debug_collector = collector.inner.clone();
    engine.on_debug(move |s, source, pos| {
        let mut state = debug_collector.lock().unwrap();
        if let Some(src) = source {
            state.stdout.push_str(&format!("[{src}] "));
        }
        if !pos.is_none() {
            state.stdout.push_str(&format!("{pos:?} | "));
        }
        state.stdout.push_str(s);
        state.stdout.push('\n');
    });

    collector
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_capture() {
        let mut engine = Engine::new();
        let collector = register_output_callbacks(&mut engine);

        engine
            .eval::<()>(r#"svg("<svg>hello</svg>")"#)
            .unwrap();

        assert_eq!(
            collector.take_svg().as_deref(),
            Some("<svg>hello</svg>")
        );
        // Second take returns None
        assert!(collector.take_svg().is_none());
    }

    #[test]
    fn print_capture() {
        let mut engine = Engine::new();
        let collector = register_output_callbacks(&mut engine);

        engine
            .eval::<()>(r#"print("line 1"); print("line 2");"#)
            .unwrap();

        let stdout = collector.take_stdout();
        assert!(stdout.contains("line 1"));
        assert!(stdout.contains("line 2"));

        // Second take returns empty
        assert!(collector.take_stdout().is_empty());
    }

    #[test]
    fn clear_resets() {
        let mut engine = Engine::new();
        let collector = register_output_callbacks(&mut engine);

        engine.eval::<()>(r#"svg("test"); print("hello");"#).unwrap();

        collector.clear();
        assert!(collector.take_svg().is_none());
        assert!(collector.take_stdout().is_empty());
    }

    #[test]
    fn concurrent_access() {
        let mut engine = Engine::new();
        let collector = register_output_callbacks(&mut engine);

        // Clone for "concurrent" access
        let c2 = collector.clone();

        engine.eval::<()>(r#"svg("<svg/>")"#).unwrap();

        // Both clones see the same state
        assert_eq!(c2.take_svg().as_deref(), Some("<svg/>"));
        assert!(collector.take_svg().is_none()); // already taken
    }
}
