//! Exercise the actual Criterion fan-out workload across successive runtime entries.
//!
//! References:
//! - `docs/architecture.md`, "RTSP server contract"

use std::time::Duration;

#[path = "../benches/support/fanout.rs"]
mod fanout;

#[test]
fn fanout_bursts_complete_across_repeated_runtime_entries() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    for clients in [1, 8, 32] {
        let mut burst = runtime.block_on(async { fanout::FanoutBurst::new(clients) });
        // Criterion repeatedly calls block_on, unlike a single async loop. Keep that seam:
        // an old pump can still be parked when the next measured iteration starts.
        for iteration in 0..100 {
            runtime
                .block_on(async { tokio::time::timeout(Duration::from_secs(2), burst.run()).await })
                .unwrap_or_else(|error| {
                    panic!(
                        "fan-out stalled for {clients} clients at iteration {iteration}: {error}"
                    )
                });
        }
    }
}
