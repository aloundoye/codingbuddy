#![allow(clippy::unwrap_used)]
//! Checkpoint cost on a workspace with many files. Run with
//! `cargo test -p codingbuddy-memory --release --test checkpoint_speed -- --ignored --nocapture`.

use codingbuddy_memory::MemoryManager;
use std::time::Instant;

#[test]
#[ignore = "benchmark"]
fn checkpoint_speed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(root)
        .status()
        .unwrap();
    std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
    for (sub, count) in [("src", 3000), ("node_modules/pkg", 5000)] {
        for i in 0..count {
            let path = root
                .join(sub)
                .join(format!("d{}", i / 100))
                .join(format!("f{i}.txt"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{i}").repeat(1000)).unwrap();
        }
    }
    let manager = MemoryManager::new(root).unwrap();
    for round in 0..3 {
        let started = Instant::now();
        let checkpoint = manager.create_checkpoint("bench").unwrap();
        let created = started.elapsed();
        let started = Instant::now();
        let changed = manager
            .changed_since_checkpoint(checkpoint.checkpoint_id)
            .unwrap();
        println!(
            "round {round}: checkpoint {created:?} ({} files), diff {:?} ({} changed)",
            checkpoint.files_count,
            started.elapsed(),
            changed.len()
        );
    }
}
