use crate::{Error, Result, domain::Id};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub success: bool,
}

pub struct ManagedProcess {
    child: Option<Child>,
    adopted_start: Option<u64>,
    preserve_on_drop: bool,
    pub pid: u32,
    pub run_id: Id,
}

impl ManagedProcess {
    pub fn spawn(
        run_id: Id,
        argv: &[String],
        stdout: Option<&Path>,
        stderr: Option<&Path>,
    ) -> Result<Self> {
        Self::spawn_inner(run_id, argv, stdout, stderr, None)
    }

    #[cfg(unix)]
    pub fn spawn_with_stdin(
        run_id: Id,
        argv: &[String],
        stdout: Option<&Path>,
        stderr: Option<&Path>,
        stdin: UnixStream,
    ) -> Result<Self> {
        Self::spawn_inner(
            run_id,
            argv,
            stdout,
            stderr,
            Some(Stdio::from(std::os::fd::OwnedFd::from(stdin))),
        )
    }

    fn spawn_inner(
        run_id: Id,
        argv: &[String],
        stdout: Option<&Path>,
        stderr: Option<&Path>,
        stdin: Option<Stdio>,
    ) -> Result<Self> {
        let executable = argv
            .first()
            .ok_or_else(|| Error::Process("empty process argv".into()))?;
        let mut command = Command::new(executable);
        command.args(argv.iter().skip(1));
        if let Some(stdin) = stdin {
            command.stdin(stdin);
        }
        if let Some(path) = stdout {
            let file = File::create(path).map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
            command.stdout(Stdio::from(file));
        } else {
            command.stdout(Stdio::null());
        }
        if let Some(path) = stderr {
            let file = File::create(path).map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })?;
            command.stderr(Stdio::from(file));
        } else {
            command.stderr(Stdio::null());
        }
        // A failed spawn is not a filesystem error about a path: the usual cause
        // is a helper that is not installed, and reporting it as one sent the
        // reader looking for a missing directory instead of a missing program.
        let child = command.spawn().map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound
                && !executable.contains(std::path::MAIN_SEPARATOR)
            {
                Error::ExecutableNotFound {
                    executable: executable.clone(),
                }
            } else {
                Error::Spawn {
                    executable: executable.clone(),
                    source,
                }
            }
        })?;
        let pid = child.id();
        Ok(Self {
            child: Some(child),
            adopted_start: None,
            pid,
            run_id,
            preserve_on_drop: false,
        })
    }

    pub fn adopt(run_id: Id, pid: u32, process_start: u64) -> Result<Self> {
        if !process_identity_matches(pid, process_start) {
            return Err(Error::Process(
                "helper process identity no longer matches".into(),
            ));
        }
        Ok(Self {
            child: None,
            adopted_start: Some(process_start),
            pid,
            run_id,
            preserve_on_drop: true,
        })
    }

    pub fn preserve_on_drop(&mut self) {
        self.preserve_on_drop = true;
    }

    pub fn try_wait(&mut self) -> Result<Option<ProcessExit>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(
                (!process_identity_matches(self.pid, self.adopted_start.unwrap_or(0))).then_some(
                    ProcessExit {
                        code: None,
                        success: true,
                    },
                ),
            );
        };
        let Some(status) = child
            .try_wait()
            .map_err(|source| Error::Process(source.to_string()))?
        else {
            return Ok(None);
        };
        Ok(Some(ProcessExit {
            code: status.code(),
            success: status.success(),
        }))
    }

    pub fn wait(&mut self) -> Result<ProcessExit> {
        if self.child.is_none() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(exit) = self.try_wait()? {
                    return Ok(exit);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(Error::Process("adopted helper did not exit".into()));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let status = self
            .child
            .as_mut()
            .expect("owned child")
            .wait()
            .map_err(|source| Error::Process(source.to_string()))?;
        Ok(ProcessExit {
            code: status.code(),
            success: status.success(),
        })
    }

    pub fn terminate(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            return child
                .kill()
                .map_err(|source| Error::Process(source.to_string()));
        }
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            )
            .map_err(|error| Error::Process(error.to_string()))
        }
        #[cfg(not(unix))]
        Err(Error::Process("adopted helpers require Unix".into()))
    }

    #[cfg(unix)]
    pub fn terminate_gracefully(&mut self) -> Result<()> {
        use nix::{
            sys::signal::{Signal, kill},
            unistd::Pid,
        };
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        kill(Pid::from_raw(self.pid as i32), Signal::SIGTERM)
            .map_err(|error| Error::Process(error.to_string()))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        self.terminate()?;
        self.wait()?;
        Ok(())
    }

    pub fn process_start(&self) -> Result<u64> {
        self.adopted_start
            .or_else(|| process_start_identity(self.pid))
            .ok_or_else(|| {
                Error::Process(format!("cannot read process identity for pid {}", self.pid))
            })
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        // A Child handle alone does not own the process on drop. Keep the
        // process tied to its supervisor even when a later start step fails.
        if !self.preserve_on_drop
            && let Some(child) = &mut self.child
            && matches!(child.try_wait(), Ok(None))
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn process_identity_matches(pid: u32, expected_start: u64) -> bool {
    process_start_identity(pid).is_some_and(|actual| actual == expected_start)
}

#[cfg(target_os = "linux")]
pub(crate) fn process_start_identity(pid: u32) -> Option<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let Ok(contents) = fs::read_to_string(path) else {
        return None;
    };
    let after_name = contents.rsplit_once(") ").map(|(_, rest)| rest)?;
    let mut fields = after_name.split_whitespace();
    if matches!(fields.next()?, "Z" | "X") {
        return None;
    }
    fields.nth(18).and_then(|value| value.parse::<u64>().ok())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_identity_matches(_pid: u32, _expected_start: u64) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_start_identity(_pid: u32) -> Option<u64> {
    None
}
