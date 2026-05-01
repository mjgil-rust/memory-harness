use std::path::Path;
use std::process::Command;
use std::time::Instant;

use tempfile::tempdir;

#[test]
fn json_output_contains_core_fields() {
    let output = Command::new(memory_harness_bin())
        .args([
            "--format",
            "json",
            "--objects-processed",
            "123",
            "--",
            "/usr/bin/env",
            "true",
        ])
        .output()
        .expect("run memory-harness");

    assert!(output.status.success(), "status was {:?}", output.status);
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(stdout.contains("\"status\": \"ok\""), "{stdout}");
    assert!(stdout.contains("\"child_max_rss_kb\""), "{stdout}");
    assert!(stdout.contains("\"harness_max_rss_kb\""), "{stdout}");
    assert!(stdout.contains("\"objects_processed\": 123"), "{stdout}");
    assert!(stdout.contains("\"objects_per_second\""), "{stdout}");
    assert!(stdout.contains("\"seconds_per_object\""), "{stdout}");
}

#[test]
fn proc_status_sampler_writes_sample_file() {
    let temp = tempdir().expect("tempdir");
    let samples = temp.path().join("samples.tsv");
    let output = Command::new(memory_harness_bin())
        .args([
            "--format",
            "json",
            "--sample-proc-status-ms",
            "1",
            "--sample-proc-status-out",
        ])
        .arg(&samples)
        .args(["--", "/bin/sh", "-c", "sleep 0.02"])
        .output()
        .expect("run memory-harness with sampler");

    assert!(output.status.success(), "status was {:?}", output.status);
    let sample_contents = std::fs::read_to_string(&samples).expect("read samples");
    assert!(sample_contents.contains("elapsed_ms"), "{sample_contents}");
}

#[test]
fn child_timeout_stops_the_run_on_the_requested_boundary() {
    let started = Instant::now();
    let output = Command::new(memory_harness_bin())
        .args([
            "--format",
            "json",
            "--child-timeout-seconds",
            "1",
            "--",
            "/bin/sh",
            "-c",
            "sleep 5",
        ])
        .output()
        .expect("run memory-harness with timeout");

    let elapsed = started.elapsed().as_secs_f64();
    assert_eq!(
        output.status.code(),
        Some(124),
        "status was {:?}",
        output.status
    );
    assert!(elapsed < 2.5, "elapsed was {elapsed}");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(stdout.contains("\"status\": \"timeout\""), "{stdout}");
    assert!(stdout.contains("\"timed_out\": true"), "{stdout}");
    assert!(stdout.contains("\"timeout_seconds\": 1"), "{stdout}");
}

#[test]
fn wrapper_repeat_and_perf_stat_create_artifacts() {
    if !perf_available() {
        return;
    }

    let temp = tempdir().expect("tempdir");
    let wrapper = repo_root().join("scripts/run-with-memory-and-perf.sh");
    let output = Command::new(&wrapper)
        .env("MEMORY_HARNESS_BIN", memory_harness_bin())
        .args(["--perf-stat", "--repeat", "2", "--out-dir"])
        .arg(temp.path())
        .args(["--", "/bin/sh", "-c", "sleep 1"])
        .output()
        .expect("run wrapper");

    assert!(output.status.success(), "status was {:?}", output.status);
    for run_name in ["run-01", "run-02"] {
        let run_dir = temp.path().join(run_name);
        assert!(run_dir.join("combined.log").is_file(), "{run_dir:?}");
        assert!(run_dir.join("memory-harness.json").is_file(), "{run_dir:?}");
        assert!(run_dir.join("perf.data").is_file(), "{run_dir:?}");
        assert!(run_dir.join("perf.report.txt").is_file(), "{run_dir:?}");
        assert!(run_dir.join("perf.stat.txt").is_file(), "{run_dir:?}");
        assert!(run_dir.join("summary.txt").is_file(), "{run_dir:?}");
        assert!(run_dir.join("sync/child.pid").is_file(), "{run_dir:?}");
    }
}

#[test]
fn wrapper_perf_stat_attaches_to_single_run() {
    if !perf_available() {
        return;
    }

    let temp = tempdir().expect("tempdir");
    let wrapper = repo_root().join("scripts/run-with-memory-and-perf.sh");
    let marker = temp.path().join("marker.txt");
    let run_dir = temp.path().join("run");
    let command = format!(
        "printf x >> {}; sleep 1",
        shell_single_quote(&marker.display().to_string())
    );
    let output = Command::new(&wrapper)
        .env("MEMORY_HARNESS_BIN", memory_harness_bin())
        .args(["--perf-stat", "--out-dir"])
        .arg(&run_dir)
        .args(["--", "/bin/sh", "-c"])
        .arg(&command)
        .output()
        .expect("run wrapper with perf stat");

    assert!(output.status.success(), "status was {:?}", output.status);
    let marker_contents = std::fs::read_to_string(&marker).expect("read marker");
    assert_eq!(marker_contents, "x", "{marker_contents:?}");
    assert!(run_dir.join("perf.stat.txt").is_file(), "{run_dir:?}");
}

#[test]
fn wrapper_profiler_handshake_covers_short_lived_commands() {
    if !perf_available() {
        return;
    }

    let temp = tempdir().expect("tempdir");
    let wrapper = repo_root().join("scripts/run-with-memory-and-perf.sh");
    let run_dir = temp.path().join("run");
    let output = Command::new(&wrapper)
        .env("MEMORY_HARNESS_BIN", memory_harness_bin())
        .args(["--out-dir"])
        .arg(&run_dir)
        .args(["--", "/usr/bin/env", "true"])
        .output()
        .expect("run wrapper with short command");

    assert!(output.status.success(), "status was {:?}", output.status);
    let summary = std::fs::read_to_string(run_dir.join("summary.txt")).expect("read summary");
    assert!(summary.contains("memory_status=0"), "{summary}");
    assert!(summary.contains("perf_record_status=0"), "{summary}");
    assert!(run_dir.join("perf.data").is_file(), "{run_dir:?}");
}

#[test]
fn wrapper_timeout_is_consistent_across_repeated_runs() {
    if !perf_available() {
        return;
    }

    let temp = tempdir().expect("tempdir");
    let wrapper = repo_root().join("scripts/run-with-memory-and-perf.sh");
    let output = Command::new(&wrapper)
        .env("MEMORY_HARNESS_BIN", memory_harness_bin())
        .args(["--timeout-seconds", "1", "--repeat", "2", "--out-dir"])
        .arg(temp.path())
        .args([
            "--",
            "/bin/sh",
            "-c",
            "i=0; while [ \"$i\" -lt 50000000 ]; do i=$((i+1)); done",
        ])
        .output()
        .expect("run wrapper with timeout");

    assert!(output.status.success(), "status was {:?}", output.status);
    for run_name in ["run-01", "run-02"] {
        let run_dir = temp.path().join(run_name);
        let summary = std::fs::read_to_string(run_dir.join("summary.txt")).expect("read summary");
        let memory_json =
            std::fs::read_to_string(run_dir.join("memory-harness.json")).expect("read json");
        assert!(summary.contains("timeout_seconds=1"), "{summary}");
        assert!(summary.contains("memory_status=124"), "{summary}");
        assert!(memory_json.contains("\"timed_out\": true"), "{memory_json}");
    }
}

fn memory_harness_bin() -> &'static str {
    env!("CARGO_BIN_EXE_memory-harness")
}

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn perf_available() -> bool {
    Command::new("perf")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
