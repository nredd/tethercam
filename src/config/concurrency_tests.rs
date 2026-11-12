//! Concurrent first-run identity regression.
//!
//! References:
//! - <https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist_noclobber>

use super::Config;

#[test]
fn concurrent_first_run_preserves_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || Config::load_or_create(&path))
        })
        .collect();
    let mut configs = Vec::new();
    for thread in threads {
        let result = thread.join().unwrap().unwrap();
        configs.push(result);
    }
    let first = configs.first().unwrap();
    for config in &configs {
        assert_eq!(first.endpoint_uuid, config.endpoint_uuid);
        assert!(
            first
                .credentials
                .password
                .constant_time_eq(&config.credentials.password)
        );
    }
}
