use std::{
    process::Command,
    time::{Duration, Instant},
};

#[test]
fn migrate_only_initializes_schema_and_exits_without_starting_workers() {
    let dir = std::env::temp_dir().join(format!("atv-migrate-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_awesometv-server"))
        .arg("--migrate-only")
        .env("AWESOMETV_DATA_DIR", &dir)
        .env("AWESOMETV_BIND", "invalid bind address")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("migration-only did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "migration-only failed: {status}");
    assert!(dir.join("awesometv.db").is_file());
    assert!(
        !dir.join("precache").exists(),
        "precache worker was started"
    );
    std::fs::remove_dir_all(dir).unwrap();
}
