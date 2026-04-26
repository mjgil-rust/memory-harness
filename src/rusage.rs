use anyhow::{Context, Result};
use std::mem::MaybeUninit;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

#[derive(Debug, Clone)]
pub struct Usage {
    pub max_rss_kb: i64,
    pub user_cpu_s: f64,
    pub system_cpu_s: f64,
    pub minor_faults: i64,
    pub major_faults: i64,
    pub voluntary_ctx_switches: i64,
    pub involuntary_ctx_switches: i64,
}

#[derive(Debug)]
pub struct WaitOutcome {
    pub status: ExitStatus,
    pub usage: Usage,
}

pub fn wait4(pid: u32) -> Result<WaitOutcome> {
    let mut status: libc::c_int = 0;
    let mut usage = MaybeUninit::<libc::rusage>::zeroed();
    let waited = unsafe {
        libc::wait4(
            pid as libc::pid_t,
            &mut status,
            0,
            usage.as_mut_ptr(),
        )
    };
    if waited < 0 {
        return Err(std::io::Error::last_os_error()).context("wait4 failed");
    }

    let usage = unsafe { usage.assume_init() };
    Ok(WaitOutcome {
        status: ExitStatus::from_raw(status),
        usage: Usage::from_rusage(usage),
    })
}

pub fn self_usage() -> Result<Usage> {
    let mut usage = MaybeUninit::<libc::rusage>::zeroed();
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getrusage(RUSAGE_SELF) failed");
    }
    Ok(Usage::from_rusage(unsafe { usage.assume_init() }))
}

impl Usage {
    fn from_rusage(raw: libc::rusage) -> Self {
        Self {
            max_rss_kb: raw.ru_maxrss,
            user_cpu_s: timeval_to_seconds(raw.ru_utime),
            system_cpu_s: timeval_to_seconds(raw.ru_stime),
            minor_faults: raw.ru_minflt,
            major_faults: raw.ru_majflt,
            voluntary_ctx_switches: raw.ru_nvcsw,
            involuntary_ctx_switches: raw.ru_nivcsw,
        }
    }
}

fn timeval_to_seconds(value: libc::timeval) -> f64 {
    value.tv_sec as f64 + (value.tv_usec as f64 / 1_000_000.0)
}
