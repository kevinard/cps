use std::{
    fs::File,
    io::{self, PipeReader, Read, Seek, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use libseccomp::{ScmpAction, ScmpFilterContext, ScmpSyscall, scmp_cmp};
use rustix::{
    io::{Errno, FdFlags, fcntl_setfd},
    process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal},
};

use super::{
    BuildWorker,
    cgroup::{self, Cgroup},
};

fn filter() -> Result<File> {
    let mut rules = ScmpFilterContext::new(ScmpAction::Allow)?;
    let denied = ScmpAction::Errno(libc::EPERM);
    for syscall in ["socket", "socketpair"] {
        rules.add_rule_conditional(
            denied,
            ScmpSyscall::from_name(syscall)?,
            &[scmp_cmp!($arg0 != libc::AF_UNIX as u64)],
        )?;
    }
    for syscall in [
        "bpf",
        "perf_event_open",
        "userfaultfd",
        "add_key",
        "keyctl",
        "request_key",
        "ptrace",
        "process_vm_readv",
        "process_vm_writev",
        "pidfd_getfd",
    ] {
        rules.add_rule(denied, ScmpSyscall::from_name(syscall)?)?;
    }
    // io_uring can create sockets without going through the socket system call.
    for syscall in ["io_uring_setup", "io_uring_enter", "io_uring_register"] {
        rules.add_rule(
            ScmpAction::Errno(libc::ENOSYS),
            ScmpSyscall::from_name(syscall)?,
        )?;
    }
    let mut file = tempfile::tempfile()?;
    rules.export_bpf(&file)?;
    file.rewind()?;
    ensure!(
        file.as_raw_fd() >= 3,
        "build requires open standard descriptors"
    );
    Ok(file)
}

pub(super) fn run(
    toolchain: &Path,
    sources: &Path,
    script: &Path,
    output: &Path,
    epoch: u32,
    worker: &BuildWorker,
) -> Result<()> {
    let group = worker.cgroup.as_ref().map(Cgroup::create).transpose()?;
    let enter = group.as_ref().map(Cgroup::processes).transpose()?;
    let killer = group.as_ref().map(Cgroup::killer).transpose()?;
    let filter = filter().context("creating seccomp build policy")?;
    let mut command = Command::new("/usr/bin/bwrap");
    command
        .env_clear()
        .args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--cap-drop",
            "ALL",
            "--die-with-parent",
            "--new-session",
            "--hostname",
            "cps-build",
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--clearenv",
            "--seccomp",
        ])
        .arg(filter.as_raw_fd().to_string())
        .arg("--ro-bind")
        .arg(toolchain)
        .arg("/")
        .arg("--ro-bind")
        .arg(sources)
        .arg("/sources")
        .arg("--ro-bind")
        .arg(script)
        .arg("/recipe/build.sh")
        .arg("--bind")
        .arg(output)
        .arg("/out")
        .args(["--proc", "/proc", "--dev", "/dev"]);
    for (path, size) in [
        ("/tmp", worker.scratch_bytes),
        ("/run", worker.scratch_bytes.min(64 << 20)),
        ("/build", worker.scratch_bytes),
    ] {
        command
            .arg("--size")
            .arg(size.to_string())
            .args(["--tmpfs", path]);
    }
    command.args(["--dir", "/build/home", "--chdir", "/build"]);
    let epoch = epoch.to_string();
    for (key, value) in [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/build/home"),
        ("USER", "builder"),
        ("LOGNAME", "builder"),
        ("HOSTNAME", "cps-build"),
        ("LC_ALL", "C"),
        ("LANG", "C"),
        ("TZ", "UTC"),
        ("TMPDIR", "/tmp"),
        ("SOURCE_DATE_EPOCH", &epoch),
        ("CPS_SOURCES", "/sources"),
        ("DESTDIR", "/out"),
    ] {
        command.args(["--setenv", key, value]);
    }
    let (logs, writer) = io::pipe()?;
    command
        .args([
            "--",
            "/bin/sh",
            "-eu",
            "-c",
            "umask 022; exec /bin/sh -eu /recipe/build.sh",
        ])
        .stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    // Only async-signal-safe system calls run here. Bubblewrap consumes the policy descriptor,
    // and the recipe's standard streams are private pipes or null.
    let file_bytes = worker.file_bytes;
    unsafe {
        command.pre_exec(move || {
            for (resource, limit) in [(libc::RLIMIT_CORE, 0), (libc::RLIMIT_FSIZE, file_bytes)] {
                let mut inherited = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(resource, &mut inherited) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let limit = libc::rlimit {
                    rlim_cur: inherited.rlim_cur.min(limit),
                    rlim_max: inherited.rlim_max.min(limit),
                };
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as _) != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(file) = &enter {
                // cgroup.procs treats 0 as the writing process. The fd is close-on-exec.
                let pid = b"0\n";
                let written = libc::write(file.as_raw_fd(), pid.as_ptr().cast(), pid.len());
                if written < 0 {
                    return Err(io::Error::last_os_error());
                }
                if written as usize != pid.len() {
                    return Err(io::ErrorKind::WriteZero.into());
                }
            }
            fcntl_setfd(&filter, FdFlags::empty())?;
            Ok(())
        });
    }
    let child = command
        .spawn()
        .context("starting rootless bubblewrap build")?;
    drop(command);
    let result = monitor(child, logs, worker, killer);
    if let Some(group) = group {
        let oom = group.oom_killed();
        group.finish()?;
        ensure!(!oom?, "isolated build exceeded its cgroup memory limit");
    }
    result
}

fn monitor(
    mut child: Child,
    mut logs: PipeReader,
    worker: &BuildWorker,
    killer: Option<File>,
) -> Result<()> {
    let (finished, watchdog) =
        match start_watchdog(&child, Duration::from_secs(worker.timeout_secs), killer) {
            Ok(watchdog) => watchdog,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.context("starting build watchdog"));
            }
        };
    let copied = relay_logs(&mut logs, &mut io::stderr().lock(), worker.log_bytes);
    if copied.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    drop(finished);
    let expired = watchdog
        .join()
        .map_err(|_| anyhow::anyhow!("build watchdog panicked"))??;
    ensure!(
        !expired,
        "isolated build exceeded its {}-second deadline",
        worker.timeout_secs
    );
    copied.context("relaying build logs")?;
    let status = status?;
    ensure!(status.success(), "isolated build failed with {status}");
    Ok(())
}

fn start_watchdog(
    child: &Child,
    timeout: Duration,
    mut killer: Option<File>,
) -> Result<(mpsc::Sender<()>, thread::JoinHandle<io::Result<bool>>)> {
    let pid = Pid::from_raw(i32::try_from(child.id())?).context("invalid sandbox PID")?;
    let pidfd = pidfd_open(pid, PidfdFlags::empty())?;
    let (finished, receiver) = mpsc::channel::<()>();
    let watchdog = thread::Builder::new()
        .name("cps-build-watchdog".to_owned())
        .spawn(move || {
            if receiver.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout) {
                let killed_group = killer.as_mut().map(cgroup::kill).transpose();
                let killed_process = pidfd_send_signal(&pidfd, Signal::KILL);
                killed_group?;
                return match killed_process {
                    Ok(()) | Err(Errno::SRCH) => Ok(true),
                    Err(error) => Err(error.into()),
                };
            }
            Ok(false)
        })?;
    Ok((finished, watchdog))
}

fn relay_logs(
    input: &mut impl Read,
    output: &mut impl Write,
    mut remaining: u64,
) -> io::Result<()> {
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let length = input.read(&mut buffer)?;
        if length == 0 {
            return Ok(());
        }
        let visible = (length as u64).min(remaining) as usize;
        for byte in &mut buffer[..visible] {
            if !byte.is_ascii_graphic() && !matches!(*byte, b' ' | b'\t' | b'\n') {
                *byte = b'?';
            }
        }
        output.write_all(&buffer[..visible])?;
        remaining -= visible as u64;
        if visible < length && !truncated {
            output.write_all(b"\n[cps: build log limit reached]\n")?;
            truncated = true;
        }
    }
}
