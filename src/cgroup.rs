use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::output::CgroupMemorySummary;

const SYS_FS_CGROUP: &str = "/sys/fs/cgroup";
const PROC_SELF_CGROUP: &str = "/proc/self/cgroup";

pub struct CgroupMemorySession {
    path: PathBuf,
}

pub struct CgroupMemorySetup {
    pub session: Option<CgroupMemorySession>,
    pub status: Option<String>,
}

impl CgroupMemorySession {
    pub fn attach_child(child_pid: u32) -> Result<Option<Self>> {
        let base = discover_memory_cgroup_base()?;
        let base = match base {
            Some(path) => path,
            None => return Ok(None),
        };

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let session_name = format!("memory-harness-{}-{}", process::id(), nanos);
        let session_path = base.join(session_name);

        fs::create_dir(&session_path)
            .with_context(|| format!("failed to create cgroup session at {}", session_path.display()))?;
        fs::write(session_path.join("cgroup.procs"), format!("{child_pid}\n"))
            .with_context(|| {
                format!(
                    "failed to move child {child_pid} into {}",
                    session_path.display()
                )
            })?;

        Ok(Some(Self { path: session_path }))
    }

    pub fn attach_child_with_reason(child_pid: u32) -> CgroupMemorySetup {
        match Self::attach_child(child_pid) {
            Ok(Some(session)) => CgroupMemorySetup {
                session: Some(session),
                status: None,
            },
            Ok(None) => CgroupMemorySetup {
                session: None,
                status: Some("cgroup v2 memory controller not available for this process".to_owned()),
            },
            Err(err) => CgroupMemorySetup {
                session: None,
                status: Some(format!("cgroup setup failed: {err:#}")),
            },
        }
    }

    pub fn collect_and_cleanup(self) -> Result<CgroupMemorySummary> {
        let usage = self.read_usage()?;
        self.cleanup()?;
        Ok(usage)
    }

    fn read_usage(&self) -> Result<CgroupMemorySummary> {
        let current_bytes = read_u64(&self.path.join("memory.current")).ok();
        let peak_bytes = read_u64(&self.path.join("memory.peak")).ok();

        let memory_stat = read_memory_stat_kib(&self.path.join("memory.stat"))?;
        let anon_kb = memory_stat.anon;
        let file_kb = memory_stat.file;
        let shmem_kb = memory_stat.shmem;
        let cache_kb = match (file_kb, shmem_kb) {
            (Some(file), Some(shmem)) => Some(file.saturating_add(shmem)),
            (Some(file), None) => Some(file),
            (None, Some(shmem)) => Some(shmem),
            _ => None,
        };
        let rss_cache_kb = match (anon_kb, cache_kb) {
            (Some(anon), Some(cache)) => Some(anon.saturating_add(cache)),
            (Some(anon), None) => Some(anon),
            (None, Some(cache)) => Some(cache),
            _ => None,
        };

        Ok(CgroupMemorySummary {
            path: Some(self.path.display().to_string()),
            current_kb: current_bytes.map(bytes_to_kib),
            peak_kb: peak_bytes.map(bytes_to_kib),
            anon_kb,
            file_kb,
            shmem_kb,
            cache_kb,
            rss_cache_kb,
        })
    }

    fn cleanup(&self) -> Result<()> {
        match fs::remove_dir(&self.path) {
            Ok(_) => Ok(()),
            Err(err) => match err.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
                _ => Err(err).with_context(|| {
                    format!("failed to remove cgroup session {}", self.path.display())
                }),
            },
        }
    }
}

impl Drop for CgroupMemorySession {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_dir(&self.path) {
            match err.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty => {}
                _ => {}
            }
        }
    }
}

fn discover_memory_cgroup_base() -> Result<Option<PathBuf>> {
    let contents =
        fs::read_to_string(PROC_SELF_CGROUP).context("failed to read /proc/self/cgroup")?;
    let mut cgroup_relative = None;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            cgroup_relative = Some(rest.to_owned());
            break;
        }
    }
    let cgroup_relative = match cgroup_relative {
        Some(path) => path,
        None => return Ok(None),
    };

    let mut base = PathBuf::from(SYS_FS_CGROUP);
    let trimmed = cgroup_relative.trim();
    if trimmed != "/" && !trimmed.is_empty() {
        base.push(trimmed.trim_start_matches('/'));
    }

    if !base.join("memory.current").is_file() {
        return Ok(None);
    }
    if !base.exists() {
        return Ok(None);
    }

    Ok(Some(base))
}

fn read_u64(path: &Path) -> Result<u64> {
    let content = fs::read_to_string(path)?;
    Ok(content
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .context("invalid cgroup memory counter format")?)
}

#[derive(Default)]
struct MemoryStatBytes {
    anon: Option<u64>,
    file: Option<u64>,
    shmem: Option<u64>,
}

fn read_memory_stat_kib(path: &Path) -> Result<MemoryStatBytes> {
    if !path.exists() {
        return Ok(MemoryStatBytes::default());
    }
    let content = fs::read_to_string(path)?;
    let mut stats = MemoryStatBytes::default();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let key = match parts.next() {
            Some(value) => value,
            None => continue,
        };
        let value = match parts.next() {
            Some(value) => value,
            None => continue,
        };
        let parsed = value.parse::<u64>().ok();
        match (key, parsed) {
            ("anon", Some(value)) => stats.anon = Some(bytes_to_kib(value)),
            ("file", Some(value)) => stats.file = Some(bytes_to_kib(value)),
            ("shmem", Some(value)) => stats.shmem = Some(bytes_to_kib(value)),
            _ => {}
        }
    }

    Ok(stats)
}

fn bytes_to_kib(bytes: u64) -> u64 {
    bytes / 1024
}
