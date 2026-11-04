use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rustix::fs::fstatfs;

use crate::image::root::Root;

pub(crate) struct CgroupLimits {
    pub root: PathBuf,
    pub cpu_percent: u32,
    pub memory_bytes: u64,
    pub tasks: u32,
}

pub(super) struct Cgroup {
    path: PathBuf,
    _parent: Root,
}

impl Cgroup {
    pub fn create(limits: &CgroupLimits) -> Result<Self> {
        let path = fs::canonicalize(&limits.root)
            .with_context(|| format!("opening delegated cgroup {}", limits.root.display()))?;
        let parent = Root::open(&path)?;
        ensure!(
            fstatfs(parent.as_file())?.f_type == libc::CGROUP2_SUPER_MAGIC,
            "{} is not a cgroup v2 directory",
            path.display()
        );
        let current = fs::read_to_string("/proc/self/cgroup")?;
        let current = current
            .lines()
            .find_map(|line| line.strip_prefix("0::/"))
            .context("finding current cgroup v2 membership")?;
        let current = fs::canonicalize(Path::new("/sys/fs/cgroup").join(current))
            .context("opening the worker's current cgroup")?;
        ensure!(
            current.starts_with(&path) && current != path,
            "the worker must run in a leaf below its delegated cgroup"
        );
        let path = tempfile::Builder::new()
            .prefix(".cps-build-")
            .tempdir_in(Root::fd_path(parent.as_file()))?
            .keep();
        let group = Self {
            path,
            _parent: parent,
        };
        let cpu = format!("{} 100000\n", u64::from(limits.cpu_percent) * 1000);
        let memory = format!("{}\n", limits.memory_bytes);
        let tasks = format!("{}\n", limits.tasks);
        for (name, value) in [
            ("cpu.max", cpu.as_str()),
            ("memory.max", memory.as_str()),
            ("memory.swap.max", "0\n"),
            ("memory.oom.group", "1\n"),
            ("pids.max", tasks.as_str()),
        ] {
            group
                .control(name)?
                .write_all(value.as_bytes())
                .with_context(|| format!("configuring build cgroup {name}"))?;
        }
        Ok(group)
    }

    pub fn processes(&self) -> Result<File> {
        self.control("cgroup.procs")
    }

    pub fn killer(&self) -> Result<File> {
        self.control("cgroup.kill")
    }

    fn control(&self, name: &str) -> Result<File> {
        File::options().write(true).open(self.path.join(name))
            .with_context(|| format!("opening build cgroup {name}; enable cpu, memory and pids on the delegated parent"))
    }

    pub fn oom_killed(&self) -> Result<bool> {
        let events = fs::read_to_string(self.path.join("memory.events"))?;
        Ok(events
            .lines()
            .filter_map(|line| line.strip_prefix("oom_kill "))
            .any(|count| count.trim() != "0"))
    }

    pub fn finish(&self) -> Result<()> {
        kill(&mut self.killer()?).context("terminating remaining build cgroup tasks")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs::remove_dir(&self.path) {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(err)
                    if err.raw_os_error() == Some(libc::EBUSY) && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(err).context("removing completed build cgroup"),
            }
        }
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // The checked cleanup path runs after the build. This also covers setup failures.
        if fs::remove_dir(&self.path).is_err()
            && let Ok(mut file) = self.killer()
        {
            let _ = kill(&mut file);
            let _ = fs::remove_dir(&self.path);
        }
    }
}

pub(super) fn kill(file: &mut File) -> io::Result<()> {
    file.write_all(b"1\n")
}
