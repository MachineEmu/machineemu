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
    child: Child,
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
        Ok(Self { child, pid, run_id })
    }

    pub fn try_wait(&mut self) -> Result<Option<ProcessExit>> {
        let Some(status) = self
            .child
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
        let status = self
            .child
            .wait()
            .map_err(|source| Error::Process(source.to_string()))?;
        Ok(ProcessExit {
            code: status.code(),
            success: status.success(),
        })
    }

    pub fn terminate(&mut self) -> Result<()> {
        self.child
            .kill()
            .map_err(|source| Error::Process(source.to_string()))
    }

    pub fn process_start(&self) -> Result<u64> {
        process_start_identity(self.pid).ok_or_else(|| {
            Error::Process(format!("cannot read process identity for pid {}", self.pid))
        })
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        // A Child handle alone does not own the process on drop. Keep the
        // process tied to its supervisor even when a later start step fails.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
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
    after_name
        .split_whitespace()
        .nth(19)
        .and_then(|value| value.parse::<u64>().ok())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_identity_matches(_pid: u32, _expected_start: u64) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn process_start_identity(_pid: u32) -> Option<u64> {
    None
}
