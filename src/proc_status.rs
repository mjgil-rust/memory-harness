use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
pub struct ProcStatusSummary {
    pub interval_ms: u64,
    pub sample_count: u64,
    pub peak_vmrss_kb: Option<u64>,
    pub peak_vmhwm_kb: Option<u64>,
    pub peak_rss_anon_kb: Option<u64>,
    pub peak_rss_file_kb: Option<u64>,
    pub peak_rss_shmem_kb: Option<u64>,
    pub peak_threads: Option<u32>,
    pub samples_path: Option<String>,
}

pub struct ProcStatusSampler {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<Result<ProcStatusSummary>>>,
}

#[derive(Copy, Clone, Debug, Default)]
struct ProcStatusSnapshot {
    vmrss_kb: Option<u64>,
    vmhwm_kb: Option<u64>,
    rss_anon_kb: Option<u64>,
    rss_file_kb: Option<u64>,
    rss_shmem_kb: Option<u64>,
    threads: Option<u32>,
}

impl ProcStatusSampler {
    pub fn start(pid: u32, interval_ms: u64, output_path: Option<PathBuf>) -> Result<Self> {
        if let Some(path) = &output_path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create sampler output directory {}", parent.display())
                })?;
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("proc-status-sampler".to_owned())
            .spawn(move || sampler_thread(stop_thread, pid, interval_ms, output_path))
            .context("failed to spawn proc-status sampler thread")?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    pub fn finish(mut self) -> Result<ProcStatusSummary> {
        self.stop.store(true, Ordering::Relaxed);
        let handle = self
            .handle
            .take()
            .context("proc-status sampler was already finished")?;
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("proc-status sampler thread panicked"))?
    }
}

fn sampler_thread(
    stop: Arc<AtomicBool>,
    pid: u32,
    interval_ms: u64,
    output_path: Option<PathBuf>,
) -> Result<ProcStatusSummary> {
    let start = Instant::now();
    let mut writer = output_path
        .as_ref()
        .map(|path| open_writer(path.as_path()))
        .transpose()?;
    if let Some(writer) = writer.as_mut() {
        writeln!(
            writer,
            "elapsed_ms\tvmrss_kb\tvmhwm_kb\trssanon_kb\trssfile_kb\trssshmem_kb\tthreads"
        )
        .context("failed to write sampler header")?;
    }

    let mut summary = ProcStatusSummary {
        interval_ms,
        sample_count: 0,
        peak_vmrss_kb: None,
        peak_vmhwm_kb: None,
        peak_rss_anon_kb: None,
        peak_rss_file_kb: None,
        peak_rss_shmem_kb: None,
        peak_threads: None,
        samples_path: output_path.as_ref().map(|path| path.display().to_string()),
    };

    loop {
        match read_proc_status(pid) {
            Ok(snapshot) => {
                summary.sample_count += 1;
                observe_peak(&mut summary.peak_vmrss_kb, snapshot.vmrss_kb);
                observe_peak(&mut summary.peak_vmhwm_kb, snapshot.vmhwm_kb);
                observe_peak(&mut summary.peak_rss_anon_kb, snapshot.rss_anon_kb);
                observe_peak(&mut summary.peak_rss_file_kb, snapshot.rss_file_kb);
                observe_peak(&mut summary.peak_rss_shmem_kb, snapshot.rss_shmem_kb);
                observe_peak_u32(&mut summary.peak_threads, snapshot.threads);
                if let Some(writer) = writer.as_mut() {
                    writeln!(
                        writer,
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        start.elapsed().as_millis(),
                        option_value(snapshot.vmrss_kb),
                        option_value(snapshot.vmhwm_kb),
                        option_value(snapshot.rss_anon_kb),
                        option_value(snapshot.rss_file_kb),
                        option_value(snapshot.rss_shmem_kb),
                        option_value(snapshot.threads.map(u64::from))
                    )
                    .context("failed to write sampler row")?;
                }
            }
            Err(err) if is_not_found(&err) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                break;
            }
            Err(err) => return Err(err),
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(interval_ms));
    }

    if let Some(mut writer) = writer {
        writer.flush().context("failed to flush sampler output")?;
    }
    Ok(summary)
}

fn open_writer(path: &Path) -> Result<BufWriter<File>> {
    let file = File::create(path)
        .with_context(|| format!("failed to create sampler output {}", path.display()))?;
    Ok(BufWriter::new(file))
}

fn read_proc_status(pid: u32) -> Result<ProcStatusSnapshot> {
    let path = format!("/proc/{pid}/status");
    let contents =
        std::fs::read_to_string(&path).with_context(|| format!("failed to read {path}"))?;
    let mut snapshot = ProcStatusSnapshot::default();
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            snapshot.vmrss_kb = parse_kb(value);
        } else if let Some(value) = line.strip_prefix("VmHWM:") {
            snapshot.vmhwm_kb = parse_kb(value);
        } else if let Some(value) = line.strip_prefix("RssAnon:") {
            snapshot.rss_anon_kb = parse_kb(value);
        } else if let Some(value) = line.strip_prefix("RssFile:") {
            snapshot.rss_file_kb = parse_kb(value);
        } else if let Some(value) = line.strip_prefix("RssShmem:") {
            snapshot.rss_shmem_kb = parse_kb(value);
        } else if let Some(value) = line.strip_prefix("Threads:") {
            snapshot.threads = parse_plain_u32(value);
        }
    }
    Ok(snapshot)
}

fn parse_kb(value: &str) -> Option<u64> {
    value
        .split_whitespace()
        .next()
        .and_then(|raw| raw.parse::<u64>().ok())
}

fn parse_plain_u32(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok()
}

fn observe_peak(slot: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        if slot.map_or(true, |current| value > current) {
            *slot = Some(value);
        }
    }
}

fn observe_peak_u32(slot: &mut Option<u32>, value: Option<u32>) {
    if let Some(value) = value {
        if slot.map_or(true, |current| value > current) {
            *slot = Some(value);
        }
    }
}

fn option_value(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}
