//! Start the loopback `nexa-server` the same way the CLI does: spawn the
//! sibling binary as a detached process and wait until it answers.

use std::{
    fmt,
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

/// Outcome of one readiness probe against the local server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerProbe {
    /// The server accepted a request.
    Ready,
    /// Nothing is listening yet.
    Connecting,
}

/// Failure to spawn the local server or wait until it is ready.
#[derive(Debug)]
pub enum LocalServerError {
    Io(io::Error),
    Failed(String),
    TimedOut {
        status: Option<ExitStatus>,
        log_path: PathBuf,
    },
}

impl fmt::Display for LocalServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Failed(message) => formatter.write_str(message),
            Self::TimedOut { status, log_path } => {
                let reason = status.map_or_else(
                    || "nexa-server did not become ready".to_owned(),
                    |status| {
                        format!("nexa-server exited with {status} and no local server became ready")
                    },
                );
                write!(formatter, "{reason}; see {}", log_path.display())
            }
        }
    }
}

impl std::error::Error for LocalServerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Failed(_) | Self::TimedOut { .. } => None,
        }
    }
}

impl From<io::Error> for LocalServerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Probe the local server; if nothing is listening, start `nexa-server` as a
/// detached background process and wait until a probe reports [`ServerProbe::Ready`].
///
/// `on_starting` runs once, immediately before spawn, so a UI can say that
/// the server is coming up. It is not called when the probe is already ready.
pub async fn ensure_local_server<F, Fut>(
    log_path: impl AsRef<Path>,
    mut probe: F,
    mut on_starting: impl FnMut(),
) -> Result<(), LocalServerError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<ServerProbe, LocalServerError>>,
{
    match probe().await? {
        ServerProbe::Ready => return Ok(()),
        ServerProbe::Connecting => {}
    }

    on_starting();
    let log_path = log_path.as_ref();
    let mut server = spawn_local_server(log_path)?;
    let mut exit_status = None;

    for _ in 0..50 {
        if exit_status.is_none() {
            exit_status = server.try_wait()?;
        }
        match probe().await? {
            ServerProbe::Ready => return Ok(()),
            ServerProbe::Connecting => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }

    stop_child(&mut server);
    Err(LocalServerError::TimedOut {
        status: exit_status,
        log_path: log_path.to_path_buf(),
    })
}

fn spawn_local_server(log_path: &Path) -> io::Result<Child> {
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let errors = log.try_clone()?;
    let mut command = Command::new(server_binary()?);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errors));
    detach(&mut command);
    command.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("could not start nexa-server: {error}"),
        )
    })
}

fn server_binary() -> io::Result<PathBuf> {
    let current_executable = std::env::current_exe()?;
    let sibling =
        current_executable.with_file_name(format!("nexa-server{}", std::env::consts::EXE_SUFFIX));
    if sibling.is_file() {
        Ok(sibling)
    } else {
        Ok(PathBuf::from("nexa-server"))
    }
}

fn stop_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn detach(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn skips_spawn_when_the_probe_is_already_ready() {
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("server.log");
        let started = AtomicUsize::new(0);
        ensure_local_server(
            &log_path,
            || async { Ok(ServerProbe::Ready) },
            || {
                started.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 0);
        assert!(!log_path.exists());
    }

    #[tokio::test]
    async fn surfaces_a_probe_failure_without_spawning() {
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("server.log");
        let error = ensure_local_server(
            &log_path,
            || async { Err(LocalServerError::Failed("boom".to_owned())) },
            || unreachable!("must not spawn on a failed probe"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "boom");
        assert!(!log_path.exists());
    }
}
