use minimonitor_core::snapshot::{MonitorSnapshot, SortMode};

/// Round-trip: parse fixture → re-serialize → parse again → assert fields.
#[test]
fn snapshot_roundtrip() {
    // Parse raw fixture JSON.
    let snap: MonitorSnapshot =
        serde_json::from_str(include_str!("fixtures/snapshot.json")).unwrap();

    // Basic sanity on top-level fields.
    assert!(
        snap.total_memory_bytes > 0,
        "total_memory_bytes must be > 0"
    );
    assert!(
        !snap.ports.is_empty(),
        "macOS fixture must have listening ports"
    );
    assert!(snap.ports[0].port > 0, "first port must be non-zero");

    // Tuple field round-trips (load_average is (f64, f64, f64)).
    let _ = snap.load_average.0;

    // Enum round-trips.
    assert_eq!(
        snap.sort_mode,
        SortMode::Cpu,
        "sort_mode must survive round-trip as 'cpu'"
    );

    // Re-serialize to JSON.
    let json2 = serde_json::to_string(&snap).expect("re-serialize failed");

    // Parse the re-serialized JSON.
    let snap2: MonitorSnapshot = serde_json::from_str(&json2).expect("second parse failed");

    // Assert key fields are identical after the round-trip.
    assert_eq!(snap2.total_memory_bytes, snap.total_memory_bytes);
    assert_eq!(snap2.used_memory_bytes, snap.used_memory_bytes);
    assert_eq!(snap2.sort_mode, snap.sort_mode);
    assert_eq!(snap2.ports.len(), snap.ports.len());
    assert_eq!(snap2.processes.len(), snap.processes.len());
    assert_eq!(snap2.cores.len(), snap.cores.len());
    assert_eq!(snap2.disks.len(), snap.disks.len());
    assert_eq!(snap2.identity.hostname, snap.identity.hostname);
    assert_eq!(snap2.load_average, snap.load_average);
    assert_eq!(snap2.uptime_secs, snap.uptime_secs);
    assert_eq!(snap2.boot_epoch, snap.boot_epoch);
    assert_eq!(
        snap2.ai_snapshot.workload_count,
        snap.ai_snapshot.workload_count
    );
}
