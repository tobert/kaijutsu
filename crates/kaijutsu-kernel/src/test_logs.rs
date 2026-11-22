//! Capture what a test logs, to assert on a log line an operator will read.

use std::sync::{Arc, Mutex};

/// A buffer holding what the current thread logs until the guard drops. A
/// log line a task logs on another thread is not captured.
pub(crate) struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    /// Everything logged so far.
    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

pub(crate) fn capture_logs() -> (Captured, tracing::subscriber::DefaultGuard) {
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = Sink(buffer.clone());
    let subscriber = tracing_subscriber::fmt().with_writer(move || sink.clone()).with_ansi(false).finish();
    (Captured(buffer), tracing::subscriber::set_default(subscriber))
}
