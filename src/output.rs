use crate::proc_status::ProcStatusSummary;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CgroupMemorySummary {
    pub path: Option<String>,
    pub current_kb: Option<u64>,
    pub peak_kb: Option<u64>,
    pub anon_kb: Option<u64>,
    pub file_kb: Option<u64>,
    pub shmem_kb: Option<u64>,
    pub cache_kb: Option<u64>,
    pub rss_cache_kb: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub timeout_seconds: Option<u64>,
    pub elapsed_s: f64,
    pub child_max_rss_kb: i64,
    pub child_max_rss_mb: f64,
    pub child_user_cpu_s: f64,
    pub child_system_cpu_s: f64,
    pub child_minor_faults: i64,
    pub child_major_faults: i64,
    pub child_voluntary_ctx_switches: i64,
    pub child_involuntary_ctx_switches: i64,
    pub harness_max_rss_kb: i64,
    pub harness_max_rss_mb: f64,
    pub proc_status: Option<ProcStatusSummary>,
    pub child_cgroup_memory: Option<CgroupMemorySummary>,
    pub child_cgroup_memory_status: Option<String>,
    pub objects_processed: Option<u64>,
    pub objects_per_second: Option<f64>,
    pub seconds_per_object: Option<f64>,
}

pub fn print_human(report: &Report) {
    println!(
        "status={} timed_out={} timeout_seconds={} elapsed_s={:.3} child_max_rss_kb={} child_max_rss_mb={:.2} exit_code={} signal={} command={:?}",
        report.status,
        report.timed_out,
        option_u64(report.timeout_seconds),
        report.elapsed_s,
        report.child_max_rss_kb,
        report.child_max_rss_mb,
        option_i32(report.exit_code),
        option_i32(report.signal),
        report.command
    );
    println!(
        "child_user_cpu_s={:.3} child_system_cpu_s={:.3} child_minor_faults={} child_major_faults={} child_voluntary_ctx_switches={} child_involuntary_ctx_switches={}",
        report.child_user_cpu_s,
        report.child_system_cpu_s,
        report.child_minor_faults,
        report.child_major_faults,
        report.child_voluntary_ctx_switches,
        report.child_involuntary_ctx_switches
    );
    println!(
        "harness_max_rss_kb={} harness_max_rss_mb={:.2} cwd={}",
        report.harness_max_rss_kb,
        report.harness_max_rss_mb,
        report.cwd.as_deref().unwrap_or("none")
    );
    if let Some(objects_processed) = report.objects_processed {
        let objects_per_second = report
            .objects_per_second
            .map(|value| format!("{value:.3}"))
            .unwrap_or_else(|| "none".to_owned());
        let seconds_per_object = report
            .seconds_per_object
            .map(|value| format!("{value:.6}"))
            .unwrap_or_else(|| "none".to_owned());
        println!(
            "objects_processed={} objects_per_second={} seconds_per_object={}",
            objects_processed,
            objects_per_second,
            seconds_per_object
        );
    }
    if let Some(proc_status) = &report.proc_status {
        println!(
            "proc_status_interval_ms={} proc_status_sample_count={} proc_status_peak_vmrss_kb={} proc_status_peak_vmhwm_kb={} proc_status_peak_rss_anon_kb={} proc_status_peak_rss_file_kb={} proc_status_peak_rss_shmem_kb={} proc_status_peak_threads={} proc_status_samples_path={}",
            proc_status.interval_ms,
            proc_status.sample_count,
            option_u64(proc_status.peak_vmrss_kb),
            option_u64(proc_status.peak_vmhwm_kb),
            option_u64(proc_status.peak_rss_anon_kb),
            option_u64(proc_status.peak_rss_file_kb),
            option_u64(proc_status.peak_rss_shmem_kb),
            option_u64(proc_status.peak_threads.map(u64::from)),
            proc_status.samples_path.as_deref().unwrap_or("none")
        );
    }

    if let Some(cgroup_memory) = &report.child_cgroup_memory {
        println!(
            "child_cgroup_memory_path={} child_cgroup_memory_current_kb={} child_cgroup_memory_peak_kb={} child_cgroup_memory_anon_kb={} child_cgroup_memory_file_kb={} child_cgroup_memory_shmem_kb={} child_cgroup_memory_cache_kb={} child_cgroup_memory_rss_cache_kb={}",
            cgroup_memory.path.as_deref().unwrap_or("none"),
            option_u64(cgroup_memory.current_kb),
            option_u64(cgroup_memory.peak_kb),
            option_u64(cgroup_memory.anon_kb),
            option_u64(cgroup_memory.file_kb),
            option_u64(cgroup_memory.shmem_kb),
            option_u64(cgroup_memory.cache_kb),
            option_u64(cgroup_memory.rss_cache_kb)
        );
    }
    if let Some(status) = &report.child_cgroup_memory_status {
        println!("child_cgroup_memory_status={}", status);
    }
}

pub fn print_json(report: &Report) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}

fn option_i32(value: Option<i32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".to_owned())
}

fn option_u64(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".to_owned())
}
