//! A process-wide capturing `log` backend for tests.
//!
//! Log arguments are only evaluated while a logger is enabled at that level, so a test that
//! never installs one leaves them uncovered. Installing here is idempotent, which keeps tests
//! that share a process (`cargo test`, unlike nextest) from racing on `log::set_logger`.
//!
//! References:
//! - `log` crate, `Log` trait

use std::sync::{Mutex, Once};

struct Capture(Mutex<Vec<String>>);

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        self.0.lock().unwrap().push(record.args().to_string());
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));

/// Install the capturing logger at warn level (once per process).
pub(super) fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        log::set_logger(&CAPTURE).unwrap();
        log::set_max_level(log::LevelFilter::Warn);
        assert!(log::logger().enabled(&log::Metadata::builder().level(log::Level::Warn).build()));
        log::logger().flush();
    });
}

/// Every captured line so far, from all tests in this process.
pub(super) fn lines() -> Vec<String> {
    CAPTURE.0.lock().unwrap().clone()
}
