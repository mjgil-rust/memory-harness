use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use output::Report;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

mod output;
mod proc_status;
mod rusage;
mod cgroup;

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Parser, Debug)]
#[command(
    name = "memory-harness",
    about = "Run one child command and report peak RSS and rusage data"
)]
struct Args {
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,

    #[arg(long)]
    cwd: Option<PathBuf>,

    #[arg(long = "env", value_name = "KEY=VALUE")]
    env_vars: Vec<String>,

    #[arg(long, default_value_t = false)]
    clear_env: bool,

    #[arg(long, value_name = "MS")]
    sample_proc_status_ms: Option<u64>,

    #[arg(long, value_name = "PATH")]
    sample_proc_status_out: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    child_stdout: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    child_stderr: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    announce_child_pid_file: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    await_profiler_file: Option<PathBuf>,

    #[arg(long, value_name = "SECONDS")]
    child_timeout_seconds: Option<u64>,

    #[arg(long, value_name = "PATH")]
    timeout_marker_file: Option<PathBuf>,

    #[arg(long, value_name = "COUNT")]
    objects_processed: Option<u64>,

    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<OsString>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    validate_profiler_sync_args(&args)?;
    let command = command_strings(&args.command);
    let (program, program_args) = args
        .command
        .split_first()
        .context("missing command after '--'")?;

    let mut child_cmd = Command::new(program);
    child_cmd.args(program_args);
    if let Some(cwd) = &args.cwd {
        child_cmd.current_dir(cwd);
    }
    if args.clear_env {
        child_cmd.env_clear();
    }
    for entry in &args.env_vars {
        let (key, value) = parse_env_assignment(entry)?;
        child_cmd.env(key, value);
    }
    if let Some(path) = &args.child_stdout {
        child_cmd.stdout(open_stdio_file(path)?);
    }
    if let Some(path) = &args.child_stderr {
        child_cmd.stderr(open_stdio_file(path)?);
    }
    let child = child_cmd.spawn().with_context(|| {
        format!(
            "failed to spawn command {:?}",
            command.first().cloned().unwrap_or_default()
        )
    })?;
    let cgroup_setup = cgroup::CgroupMemorySession::attach_child_with_reason(child.id());
    let measurement_started = if args.announce_child_pid_file.is_some() {
        stop_child(child.id())?;
        write_child_pid_file(
            args.announce_child_pid_file
                .as_ref()
                .context("missing child pid file path")?,
            child.id(),
        )?;
        wait_for_file(
            args.await_profiler_file
                .as_ref()
                .context("missing profiler release path")?,
            Duration::from_secs(30),
        )?;
        continue_child(child.id())?;
        Instant::now()
    } else {
        Instant::now()
    };
    let sampler_interval_ms = args
        .sample_proc_status_ms
        .or_else(|| args.sample_proc_status_out.as_ref().map(|_| 10));
    let sampler = sampler_interval_ms
        .map(|interval_ms| {
            proc_status::ProcStatusSampler::start(
                child.id(),
                interval_ms,
                args.sample_proc_status_out.clone(),
            )
        })
        .transpose()?;
    let timeout_watch = args
        .child_timeout_seconds
        .map(|seconds| start_child_timeout_watch(child.id(), Duration::from_secs(seconds)));
    let outcome = rusage::wait4(child.id()).context("failed while waiting for child process")?;
    let mut child_cgroup_memory = None;
    let mut child_cgroup_memory_status = cgroup_setup.status;
    if let Some(session) = cgroup_setup.session {
        match session.collect_and_cleanup() {
            Ok(memory) => child_cgroup_memory = Some(memory),
            Err(err) => {
                child_cgroup_memory_status =
                    Some(format!("cgroup collection failed: {err:#}"));
            }
        }
    }
    let timed_out = timeout_watch
        .as_ref()
        .is_some_and(ChildTimeoutWatch::was_triggered)
        || args
            .timeout_marker_file
            .as_ref()
            .is_some_and(|path| path.exists());
    if let Some(watch) = timeout_watch {
        watch.cancel_and_join()?;
    }
    let elapsed_s = measurement_started.elapsed().as_secs_f64();
    let harness_usage = rusage::self_usage()?;
    let proc_status = sampler.map(|sampler| sampler.finish()).transpose()?;
    let status = outcome.status;

    let report = Report {
        command,
        cwd: args.cwd.as_ref().map(|p| p.display().to_string()),
        status: status_string(status, timed_out),
        exit_code: status.code(),
        signal: status.signal(),
        timed_out,
        timeout_seconds: args.child_timeout_seconds,
        elapsed_s,
        child_max_rss_kb: outcome.usage.max_rss_kb,
        child_max_rss_mb: kib_to_mib(outcome.usage.max_rss_kb),
        child_user_cpu_s: outcome.usage.user_cpu_s,
        child_system_cpu_s: outcome.usage.system_cpu_s,
        child_minor_faults: outcome.usage.minor_faults,
        child_major_faults: outcome.usage.major_faults,
        child_voluntary_ctx_switches: outcome.usage.voluntary_ctx_switches,
        child_involuntary_ctx_switches: outcome.usage.involuntary_ctx_switches,
        harness_max_rss_kb: harness_usage.max_rss_kb,
        harness_max_rss_mb: kib_to_mib(harness_usage.max_rss_kb),
        proc_status,
        child_cgroup_memory,
        child_cgroup_memory_status,
        objects_processed: args.objects_processed,
        objects_per_second: throughput_metrics(
            args.objects_processed,
            elapsed_s,
        ),
        seconds_per_object: elapsed_per_object(
            args.objects_processed,
            elapsed_s,
        ),
    };

    match args.format {
        OutputFormat::Human => output::print_human(&report),
        OutputFormat::Json => output::print_json(&report)?,
    }

    if timed_out {
        std::process::exit(124)
    } else if status.success() {
        Ok(())
    } else if let Some(code) = status.code() {
        std::process::exit(code)
    } else if let Some(signal) = status.signal() {
        std::process::exit(128 + signal)
    } else {
        bail!("child process terminated without a reportable status")
    }
}

fn parse_env_assignment(input: &str) -> Result<(&str, &str)> {
    let Some((key, value)) = input.split_once('=') else {
        bail!("invalid --env value {input:?}; expected KEY=VALUE");
    };
    if key.is_empty() {
        bail!("invalid --env value {input:?}; key must not be empty");
    }
    Ok((key, value))
}

fn command_strings(values: &[OsString]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}

fn status_string(status: std::process::ExitStatus, timed_out: bool) -> String {
    if timed_out {
        "timeout".to_owned()
    } else if status.success() {
        "ok".to_owned()
    } else if let Some(code) = status.code() {
        format!("exit({code})")
    } else if let Some(signal) = status.signal() {
        format!("signal({signal})")
    } else {
        "unknown".to_owned()
    }
}

fn kib_to_mib(value: i64) -> f64 {
    value as f64 / 1024.0
}

fn throughput_metrics(objects_processed: Option<u64>, elapsed_s: f64) -> Option<f64> {
    objects_processed.map(|objects| (objects as f64) / elapsed_s)
}

fn elapsed_per_object(objects_processed: Option<u64>, elapsed_s: f64) -> Option<f64> {
    objects_processed.and_then(|objects| {
        if objects == 0 {
            None
        } else {
            Some(elapsed_s / (objects as f64))
        }
    })
}

fn open_stdio_file(path: &PathBuf) -> Result<Stdio> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create child stdio directory {}",
                parent.display()
            )
        })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open child stdio file {}", path.display()))?;
    Ok(Stdio::from(file))
}

fn validate_profiler_sync_args(args: &Args) -> Result<()> {
    let pid_file = args.announce_child_pid_file.is_some();
    let await_file = args.await_profiler_file.is_some();
    if pid_file != await_file {
        bail!("--announce-child-pid-file and --await-profiler-file must be provided together");
    }
    Ok(())
}

fn write_child_pid_file(path: &PathBuf, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, format!("{pid}\n"))
        .with_context(|| format!("failed to write child pid file {}", path.display()))
}

fn wait_for_file(path: &PathBuf, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(2));
    }
    bail!(
        "timed out waiting for profiler release file {}",
        path.display()
    )
}

fn stop_child(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to stop child for profiler attach");
    }
    Ok(())
}

fn continue_child(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to continue child after profiler attach");
    }
    Ok(())
}

struct ChildTimeoutWatch {
    cancel: Arc<AtomicBool>,
    triggered: Arc<AtomicBool>,
    handle: thread::JoinHandle<Result<()>>,
}

impl ChildTimeoutWatch {
    fn was_triggered(&self) -> bool {
        self.triggered.load(Ordering::SeqCst)
    }

    fn cancel_and_join(self) -> Result<()> {
        self.cancel.store(true, Ordering::SeqCst);
        match self.handle.join() {
            Ok(result) => result,
            Err(_) => bail!("child timeout watch thread panicked"),
        }
    }
}

fn start_child_timeout_watch(pid: u32, timeout: Duration) -> ChildTimeoutWatch {
    let cancel = Arc::new(AtomicBool::new(false));
    let triggered = Arc::new(AtomicBool::new(false));
    let cancel_thread = Arc::clone(&cancel);
    let triggered_thread = Arc::clone(&triggered);
    let handle = thread::spawn(move || {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if cancel_thread.load(Ordering::SeqCst) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        if cancel_thread.load(Ordering::SeqCst) {
            return Ok(());
        }
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                return Err(err).context("failed to kill timed out child");
            }
        }
        triggered_thread.store(true, Ordering::SeqCst);
        Ok(())
    });
    ChildTimeoutWatch {
        cancel,
        triggered,
        handle,
    }
}
