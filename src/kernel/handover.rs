use std::{
    ffi::{CString, OsString},
    fs::{self, File, OpenOptions},
    io,
    mem::size_of,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{fs::FileTypeExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process,
};

use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use rustix::{
    fs::{fcntl_get_seals, fstatfs},
    io::{FdFlags, fcntl_setfd},
};

use super::pinned_file;

// Linux v7.3 include/uapi/linux/liveupdate.h uses _IO(0xBA, command).
const CREATE_SESSION: libc::c_ulong = 0xba00;
const RETRIEVE_SESSION: libc::c_ulong = 0xba01;
const PRESERVE_FD: libc::c_ulong = 0xba40;
const RETRIEVE_FD: libc::c_ulong = 0xba41;
const FINISH: libc::c_ulong = 0xba42;
const STATE_TOKEN: u64 = 1;
const KEXEC_FILE_UNLOAD: libc::c_ulong = 1;
const RAMFS_MAGIC: u32 = 0x858458f6;
const LUO_DEVICE: &str = "/dev/liveupdate";

#[derive(Subcommand)]
pub(crate) enum Command {
    /// From an initramfs PID 1, preserve a memfd and start the target kernel
    Start(Start),
    /// Recover a prior session and exec a program with CPS_HANDOVER_FD set
    Receive {
        /// Name of the preserved session
        #[arg(long)]
        session: String,
        /// Absolute program path followed by its arguments
        #[arg(last = true, required = true, num_args = 1..)]
        program: Vec<OsString>,
    },
}

#[derive(Args)]
pub(crate) struct Start {
    /// Name the receiving initramfs will use to recover this session
    #[arg(long)]
    session: String,
    /// Inherited memfd number, at least 3, holding the workload state
    #[arg(long)]
    state_fd: RawFd,
    /// Path to the target kernel's bzImage
    #[arg(long)]
    kernel: PathBuf,
    /// SHA-256 of the target kernel from a trusted source
    #[arg(long)]
    kernel_sha256: String,
    /// Path to the target initramfs containing the receiver
    #[arg(long)]
    initrd: PathBuf,
    /// SHA-256 of the target initramfs from a trusted source
    #[arg(long)]
    initrd_sha256: String,
    /// Target kernel command line including kho=on and liveupdate=on
    #[arg(long)]
    cmdline: String,
}

impl Command {
    pub(crate) fn run(self) -> Result<()> {
        ensure!(
            cfg!(target_arch = "x86_64"),
            "the kernel handover prototype requires x86-64"
        );
        match self {
            Self::Start(args) => start(args),
            Self::Receive { session, program } => receive(&session, &program),
        }
    }
}

fn start(args: Start) -> Result<()> {
    // The direct kernel call bypasses regular system shutdown. In this prototype
    // the initramfs owns workload quiescing and never mounts a system disk.
    ensure!(
        process::id() == 1,
        "start kernel handover as PID 1 in a disposable initramfs"
    );
    let root_type = fstatfs(File::open("/")?)?.f_type;
    ensure!(
        root_type as u32 == RAMFS_MAGIC || root_type == libc::TMPFS_MAGIC,
        "kernel handover requires a RAM-backed root in the prototype"
    );
    ensure!(
        args.state_fd >= 3,
        "the inherited memfd must be file descriptor 3 or higher"
    );
    // SAFETY: fcntl checks the inherited number and returns a distinct owned FD.
    let copied_fd = unsafe { libc::fcntl(args.state_fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copied_fd < 0 {
        return Err(io::Error::last_os_error()).context("duplicating the inherited workload memfd");
    }
    // SAFETY: fcntl returned a new file descriptor owned by this call.
    let workload = unsafe { OwnedFd::from_raw_fd(copied_fd) };
    fcntl_get_seals(&workload).context("checking the inherited workload memfd")?;
    require_options(&args.cmdline)?;
    let kernel = pinned_file(&args.kernel, &args.kernel_sha256)?;
    let initrd = pinned_file(&args.initrd, &args.initrd_sha256)?;
    let device = open_device()?;
    let session = session(&device, CREATE_SESSION, &args.session)?;
    let mut state = StateFd {
        size: size_of::<StateFd>() as u32,
        fd: workload.as_raw_fd(),
        token: STATE_TOKEN,
    };
    ioctl(&session, PRESERVE_FD, &mut state)
        .context("preserving the inherited memfd through LUO")?;

    load(&kernel, &initrd, &args.cmdline)?;
    eprintln!("Starting kernel handover for LUO session {}", args.session);
    // SAFETY: at this point the initramfs has registered the state and loaded the target.
    let result = unsafe { libc::reboot(libc::LINUX_REBOOT_CMD_KEXEC) };
    let error = if result < 0 {
        io::Error::last_os_error()
    } else {
        io::Error::other("kexec returned without switching kernels")
    };
    match unload() {
        Ok(()) => Err(error).context("starting the loaded kexec target"),
        Err(unload_error) => Err(error).context(format!(
            "kexec returned, and unloading its target failed: {unload_error:#}"
        )),
    }
}

pub(crate) fn require_options(cmdline: &str) -> Result<()> {
    let args = cmdline
        .split_ascii_whitespace()
        .take_while(|arg| *arg != "--");
    for option in ["kho", "liveupdate"] {
        ensure!(
            args.clone()
                .filter_map(|arg| arg.split_once('='))
                .filter_map(|(key, value)| (key == option).then_some(value))
                .eq(["on"]),
            "the kernel command line must set {option}=on once"
        );
    }
    Ok(())
}

pub(crate) fn check_staged_support() -> Result<()> {
    ensure!(
        cfg!(target_arch = "x86_64"),
        "staged handover requires x86-64"
    );
    require_options(&fs::read_to_string("/proc/cmdline")?)?;
    require_luo()?;
    ensure!(
        rustix::fs::access("/run/initramfs/shutdown", rustix::fs::Access::EXEC_OK).is_err(),
        "staged handover requires systemd to finish the shutdown without an exitrd"
    );
    require_no_target()
}

pub(crate) fn require_luo() -> Result<()> {
    // Opening the device takes exclusive control and may deserialize sessions.
    // Leave the LUO device to the service manager on a staged handover.
    ensure!(
        fs::metadata(LUO_DEVICE).is_ok_and(|metadata| metadata.file_type().is_char_device())
            && Path::new("/sys/class/misc/liveupdate").is_dir(),
        "the kernel does not expose the Live Update Orchestrator ({LUO_DEVICE})"
    );
    Ok(())
}

fn require_no_target() -> Result<()> {
    ensure!(
        fs::read_to_string("/sys/kernel/kexec_loaded")?.trim() == "0",
        "a kexec target is already loaded"
    );
    Ok(())
}

pub(crate) fn load(kernel: &File, initrd: &File, cmdline: &str) -> Result<()> {
    require_no_target()?;
    let cmdline = CString::new(cmdline)?;
    // SAFETY: both checked FDs and the NUL-terminated command line outlive the call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_kexec_file_load,
            kernel.as_raw_fd(),
            initrd.as_raw_fd(),
            cmdline.as_bytes_with_nul().len(),
            cmdline.as_ptr(),
            0 as libc::c_ulong,
        )
    };
    syscall_result(result).context("loading the checked kexec target with KHO")
}

pub(crate) fn unload() -> Result<()> {
    // SAFETY: KEXEC_FILE_UNLOAD ignores both descriptor arguments and the command line.
    let result = unsafe {
        libc::syscall(
            libc::SYS_kexec_file_load,
            -1i32,
            -1i32,
            0usize,
            std::ptr::null::<libc::c_char>(),
            KEXEC_FILE_UNLOAD,
        )
    };
    syscall_result(result).context("unloading the checked kexec target")
}

fn syscall_result(result: libc::c_long) -> io::Result<()> {
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn receive(name: &str, program: &[OsString]) -> Result<()> {
    let executable = Path::new(&program[0]);
    ensure!(
        executable.is_absolute(),
        "the receiving program requires an absolute path"
    );
    rustix::fs::access(executable, rustix::fs::Access::EXEC_OK)
        .with_context(|| format!("checking receiver {}", executable.display()))?;
    let device = open_device()?;
    let session = session(&device, RETRIEVE_SESSION, name)?;
    let mut state = StateFd {
        size: size_of::<StateFd>() as u32,
        fd: -1,
        token: STATE_TOKEN,
    };
    ioctl(&session, RETRIEVE_FD, &mut state).context("retrieving the preserved memfd from LUO")?;
    let memfd = received_fd(state.fd)?;
    fcntl_setfd(&memfd, FdFlags::empty()).context("making the restored memfd inheritable")?;
    let mut finish = Finish {
        size: size_of::<Finish>() as u32,
        reserved: 0,
    };
    ioctl(&session, FINISH, &mut finish).context("finishing the restored LUO session")?;
    drop(session);
    drop(device);
    Err(process::Command::new(executable)
        .args(&program[1..])
        .env("CPS_HANDOVER_FD", memfd.as_raw_fd().to_string())
        .exec())
    .with_context(|| {
        format!(
            "executing receiver {} with the restored memfd",
            executable.display()
        )
    })
}

#[repr(C)]
struct Session {
    size: u32,
    fd: i32,
    name: [u8; 64],
}

#[repr(C)]
struct StateFd {
    size: u32,
    fd: i32,
    token: u64,
}

#[repr(C)]
struct Finish {
    size: u32,
    reserved: u32,
}

fn open_device() -> Result<File> {
    OpenOptions::new().read(true).write(true).open(LUO_DEVICE)
        .context("opening /dev/liveupdate (requires LUO, KHO, and the liveupdate=on and kho=on boot options)")
}

fn session(device: &File, request: libc::c_ulong, name: &str) -> Result<OwnedFd> {
    ensure!(
        !name.is_empty() && name.len() < 64 && !name.contains('\0'),
        "LUO session name must be 1 to 63 bytes without NULs"
    );
    let mut args = Session {
        size: size_of::<Session>() as u32,
        fd: -1,
        name: [0; 64],
    };
    args.name[..name.len()].copy_from_slice(name.as_bytes());
    ioctl(device, request, &mut args).with_context(|| format!("opening LUO session {name:?}"))?;
    received_fd(args.fd)
}

fn received_fd(fd: RawFd) -> Result<OwnedFd> {
    ensure!(fd >= 0, "LUO did not return a file descriptor");
    // SAFETY: on success the LUO ioctl installed a new FD owned by the caller.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ioctl<T>(fd: &impl AsRawFd, request: libc::c_ulong, data: &mut T) -> io::Result<()> {
    // SAFETY: the callers pair each request with its Linux UAPI C-layout struct.
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), request, std::ptr::from_mut(data)) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
