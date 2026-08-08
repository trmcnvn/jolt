use std::hint::black_box;
use std::time::{Duration, Instant};

use jolt_engine::{DiffProjection, DiffSnapshot};
use jolt_proto::{DiffFileSummary, VcsKind};
use memchr::{memchr, memchr_iter};

fn fixture() -> DiffSnapshot {
    let mut patch = String::with_capacity(3 * 1024 * 1024);
    let mut files = Vec::new();
    let mut index = 0;
    while patch.len() < 3 * 1024 * 1024 {
        let path = format!("src/file_{index}.rs");
        patch.push_str(&format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,22 +1,22 @@\n"
        ));
        for line in 0..22 {
            patch.push_str(&format!(
                " context line {line} with identifiers and ordinary source text\n"
            ));
        }
        files.push(DiffFileSummary {
            path,
            old_path: None,
            status: "modified".into(),
            additions: 0,
            deletions: 0,
            binary: false,
        });
        index += 1;
    }
    DiffSnapshot {
        vcs: VcsKind::Git,
        label: None,
        branch: "main".into(),
        head_sha: Some("head".into()),
        patch,
        files,
        additions: 0,
        deletions: 0,
        truncated: false,
        checksum: "fixture".into(),
    }
}

fn scalar_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

fn simd_newlines(bytes: &[u8]) -> usize {
    memchr_iter(b'\n', bytes).count()
}

fn slice_contains_nul(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn simd_contains_nul(bytes: &[u8]) -> bool {
    memchr(0, bytes).is_some()
}

fn measure(mut operation: impl FnMut(), minimum: Duration) -> (u64, Duration) {
    let started = Instant::now();
    let mut iterations = 0;
    while started.elapsed() < minimum {
        operation();
        iterations += 1;
    }
    (iterations, started.elapsed())
}

fn main() {
    let snapshot = fixture();
    let bytes = snapshot.patch.as_bytes();
    let minimum = Duration::from_millis(750);
    let (scalar_n, scalar_time) = measure(
        || {
            black_box(scalar_newlines(black_box(bytes)));
        },
        minimum,
    );
    let (simd_n, simd_time) = measure(
        || {
            black_box(simd_newlines(black_box(bytes)));
        },
        minimum,
    );
    let (slice_nul_n, slice_nul_time) = measure(
        || {
            black_box(slice_contains_nul(black_box(bytes)));
        },
        minimum,
    );
    let (simd_nul_n, simd_nul_time) = measure(
        || {
            black_box(simd_contains_nul(black_box(bytes)));
        },
        minimum,
    );
    let (catalog_n, catalog_time) = measure(
        || {
            black_box(DiffProjection::build(
                "checkout",
                "device",
                "/repo",
                black_box(&snapshot),
                chrono::Utc::now(),
            ));
        },
        minimum,
    );
    println!(
        "fixture bytes={} files={}",
        bytes.len(),
        snapshot.files.len()
    );
    println!(
        "scalar newline scan: {:?}/iteration",
        scalar_time / scalar_n as u32
    );
    println!(
        "SIMD newline scan:   {:?}/iteration",
        simd_time / simd_n as u32
    );
    println!(
        "slice NUL scan:      {:?}/iteration",
        slice_nul_time / slice_nul_n as u32
    );
    println!(
        "SIMD NUL scan:       {:?}/iteration",
        simd_nul_time / simd_nul_n as u32
    );
    println!(
        "catalog build:       {:?}/iteration",
        catalog_time / catalog_n as u32
    );
    assert_eq!(scalar_newlines(bytes), simd_newlines(bytes));
    assert_eq!(slice_contains_nul(bytes), simd_contains_nul(bytes));
}
