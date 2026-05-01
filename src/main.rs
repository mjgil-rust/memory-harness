use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use output::Report;
use std::ffi::{CString, OsString};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod cgroup;
mod output;
mod proc_status;
mod rusage;

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

    let child_pid = spawn_child(&args, program, program_args, &command)?;
    let cgroup_setup = cgroup::CgroupMemorySession::attach_child_with_reason(child_pid);
    let measurement_started = if args.announce_child_pid_file.is_some() {
        wait_for_child_stop(child_pid, Duration::from_secs(1))?;
        write_child_pid_file(
            args.announce_child_pid_file
                .as_ref()
                .context("missing child pid file path")?,
            child_pid,
        )?;
        wait_for_file(
            args.await_profiler_file
                .as_ref()
                .context("missing profiler release path")?,
            Duration::from_secs(30),
        )?;
        continue_child(child_pid)?;
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
                child_pid,
                interval_ms,
                args.sample_proc_status_out.clone(),
            )
        })
        .transpose()?;
    let timeout_watch = args
        .child_timeout_seconds
        .map(|seconds| start_child_timeout_watch(child_pid, Duration::from_secs(seconds)));
    let outcome = rusage::wait4(child_pid).context("failed while waiting for child process")?;
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
        objects_per_second: throughput_metrics(args.objects_processed, elapsed_s),
        seconds_per_object: elapsed_per_object(args.objects_processed, elapsed_s),
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

fn spawn_child(
    args: &Args,
    program: &OsString,
    program_args: &[OsString],
    command: &[String],
) -> Result<u32> {
    if args.announce_child_pid_file.is_some() {
        spawn_child_for_profiler_attach(args, program, program_args)
    } else {
        spawn_child_with_command(args, program, program_args, command)
    }
}

fn spawn_child_with_command(
    args: &Args,
    program: &OsString,
    program_args: &[OsString],
    command: &[String],
) -> Result<u32> {
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
    Ok(child.id())
}

fn spawn_child_for_profiler_attach(
    args: &Args,
    program: &OsString,
    program_args: &[OsString],
) -> Result<u32> {
    let stdout_file = args
        .child_stdout
        .as_ref()
        .map(open_child_output_file)
        .transpose()?;
    let stderr_file = args
        .child_stderr
        .as_ref()
        .map(open_child_output_file)
        .transpose()?;
    let cwd = args.cwd.as_ref().map(path_to_cstring).transpose()?;
    let env_pairs = args
        .env_vars
        .iter()
        .map(|entry| {
            let (key, value) = parse_env_assignment(entry)?;
            Ok((cstring_from_bytes(key.as_bytes())?, cstring_from_bytes(value.as_bytes())?))
        })
        .collect::<Result<Vec<_>>>()?;
    let argv = build_exec_argv(program, program_args)?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error()).context("failed to fork child process");
    }
    if pid == 0 {
        child_exec_for_profiler_attach(args.clear_env, cwd, env_pairs, stdout_file, stderr_file, argv);
    }
    Ok(pid as u32)
}

fn child_exec_for_profiler_attach(
    clear_env: bool,
    cwd: Option<CString>,
    env_pairs: Vec<(CString, CString)>,
    stdout_file: Option<File>,
    stderr_file: Option<File>,
    argv: Vec<CString>,
) -> ! {
    if redirect_child_fd(stdout_file.as_ref(), libc::STDOUT_FILENO).is_err() {
        child_fatal_exit(b"memory-harness: failed to redirect child stdout\n");
    }
    if redirect_child_fd(stderr_file.as_ref(), libc::STDERR_FILENO).is_err() {
        child_fatal_exit(b"memory-harness: failed to redirect child stderr\n");
    }
    if let Some(cwd) = cwd.as_ref() {
        let rc = unsafe { libc::chdir(cwd.as_ptr()) };
        if rc != 0 {
            child_fatal_exit(b"memory-harness: failed to change child cwd\n");
        }
    }
    if clear_env {
        let rc = unsafe { libc::clearenv() };
        if rc != 0 {
            child_fatal_exit(b"memory-harness: failed to clear child environment\n");
        }
    }
    for (key, value) in &env_pairs {
        let rc = unsafe { libc::setenv(key.as_ptr(), value.as_ptr(), 1) };
        if rc != 0 {
            child_fatal_exit(b"memory-harness: failed to set child environment\n");
        }
    }
    let rc = unsafe { libc::raise(libc::SIGSTOP) };
    if rc != 0 {
        child_fatal_exit(b"memory-harness: failed to stop child before profiler attach\n");
    }
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    unsafe {
        libc::execvp(argv[0].as_ptr(), argv_ptrs.as_ptr());
    }
    child_fatal_exit(b"memory-harness: failed to exec child command\n");
}

fn redirect_child_fd(file: Option<&File>, target_fd: libc::c_int) -> Result<()> {
    if let Some(file) = file {
        let rc = unsafe { libc::dup2(file.as_raw_fd(), target_fd) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error()).context("dup2 failed");
        }
    }
    Ok(())
}

fn child_fatal_exit(message: &[u8]) -> ! {
    let _ = unsafe { libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len()) };
    unsafe { libc::_exit(127) }
}

fn build_exec_argv(program: &OsString, program_args: &[OsString]) -> Result<Vec<CString>> {
    let mut argv = Vec::with_capacity(program_args.len() + 1);
    argv.push(cstring_from_bytes(program.as_os_str().as_bytes())?);
    for arg in program_args {
        argv.push(cstring_from_bytes(arg.as_os_str().as_bytes())?);
    }
    Ok(argv)
}

fn path_to_cstring(path: &PathBuf) -> Result<CString> {
    cstring_from_bytes(path.as_os_str().as_bytes())
}

fn cstring_from_bytes(bytes: &[u8]) -> Result<CString> {
    CString::new(bytes).map_err(|_| anyhow!("argument contains an interior NUL byte"))
}

fn open_stdio_file(path: &PathBuf) -> Result<Stdio> {
    let file = open_child_output_file(path)?;
    Ok(Stdio::from(file))
}

fn open_child_output_file(path: &PathBuf) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create child stdio directory {}",
                parent.display()
            )
        })?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open child stdio file {}", path.display()))
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

fn wait_for_child_stop(pid: u32, timeout: Duration) -> Result<()> {
    let proc_status_path = PathBuf::from(format!("/proc/{pid}/status"));
    let started = Instant::now();
    while started.elapsed() < timeout {
        match std::fs::read_to_string(&proc_status_path) {
            Ok(status) => match child_state_code(&status) {
                Some("T" | "t") => return Ok(()),
                Some("Z" | "X" | "x") => {
                    bail!("child exited before profiler attach completed");
                }
                _ => {}
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!("child exited before profiler attach completed");
            }
            Err(err) => {
                return Err(err).context("failed to inspect child state for profiler attach");
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    bail!("timed out waiting for child to reach stopped state for profiler attach")
}

fn child_is_stopped(proc_status: &str) -> bool {
    matches!(child_state_code(proc_status), Some("T" | "t"))
}

fn child_state_code(proc_status: &str) -> Option<&str> {
    proc_status
        .lines()
        .find_map(|line| line.strip_prefix("State:"))
        .and_then(|state| state.split_whitespace().next())
}

#[cfg(test)]
mod profiler_sync_tests {
    use super::{child_is_stopped, child_state_code};

    #[test]
    fn child_is_stopped_detects_linux_stop_states() {
        assert!(child_is_stopped("Name:\ttest\nState:\tT (stopped)\n"));
        assert!(child_is_stopped("Name:\ttest\nState:\tt (tracing stop)\n"));
        assert!(!child_is_stopped("Name:\ttest\nState:\tS (sleeping)\n"));
    }

    #[test]
    fn child_state_code_reads_linux_proc_status() {
        assert_eq!(child_state_code("Name:\ttest\nState:\tT (stopped)\n"), Some("T"));
        assert_eq!(child_state_code("Name:\ttest\nState:\tZ (zombie)\n"), Some("Z"));
        assert_eq!(child_state_code("Name:\ttest\nThreads:\t1\n"), None);
    }
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
