use super::Config;

#[test]
fn concurrent_first_run_preserves_identity() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || Config::load_or_create(&path))
        })
        .collect();
    let mut configs = Vec::new();
    for thread in threads {
        let result = thread.join().map_err(|_| "thread failed")??;
        configs.push(result);
    }
    let first = configs.first().ok_or("no configurations")?;
    for config in &configs {
        assert_eq!(first.endpoint_uuid, config.endpoint_uuid);
        assert!(
            first
                .credentials
                .password
                .constant_time_eq(&config.credentials.password)
        );
    }
    Ok(())
}
