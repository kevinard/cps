use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};
use libseccomp::{ScmpAction, ScmpFilterContext, ScmpSyscall, scmp_cmp};
use rustix::io::{FdFlags, fcntl_setfd};

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
) -> Result<()> {
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
        .args([
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run",
            "--tmpfs",
            "/build",
            "--dir",
            "/build/home",
            "--chdir",
            "/build",
        ]);
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
    let (mut logs, writer) = io::pipe()?;
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
    unsafe {
        command.pre_exec(move || {
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &core) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as _) != 0 {
                return Err(io::Error::last_os_error());
            }
            fcntl_setfd(&filter, FdFlags::empty())?;
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("starting rootless bubblewrap build")?;
    drop(command);
    let copied = relay_logs(&mut logs, &mut io::stderr().lock());
    if copied.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    copied.context("relaying build logs")?;
    ensure!(status.success(), "isolated build failed with {status}");
    Ok(())
}

fn relay_logs(input: &mut impl Read, output: &mut impl Write) -> io::Result<()> {
    let mut buffer = [0u8; 8192];
    loop {
        let length = input.read(&mut buffer)?;
        if length == 0 {
            return Ok(());
        }
        for byte in &mut buffer[..length] {
            if !byte.is_ascii_graphic() && !matches!(*byte, b' ' | b'\t' | b'\n') {
                *byte = b'?';
            }
        }
        output.write_all(&buffer[..length])?;
    }
}
