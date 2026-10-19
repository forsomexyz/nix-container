//! Shared host process execution; no container-runtime policy lives here.
use anyhow::{bail, Context, Result};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGTERM},
    iterator::Signals,
};
use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}
impl CommandSpec {
    pub fn launch(&self, credentials: &BTreeMap<String, String>, op_file: Option<&str>) -> Command {
        let mut command = if let Some(file) = op_file {
            let mut cmd = Command::new("op");
            cmd.args(["run", "--no-masking", &format!("--env-file={file}"), "--"])
                .arg(&self.program);
            cmd
        } else {
            Command::new(&self.program)
        };
        command.args(&self.args).envs(credentials);
        command
    }
}

pub fn executable(name: &str) -> Option<PathBuf> {
    let executable = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        return executable(Path::new(name)).then(|| name.into());
    }
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(name))
        .find(|p| executable(p))
}

pub fn output(program: impl AsRef<OsStr>, args: &[&str]) -> Result<Output> {
    Command::new(program.as_ref())
        .args(args)
        .output()
        .with_context(|| format!("could not execute {}", program.as_ref().to_string_lossy()))
}

pub fn text(program: &str, args: &[&str]) -> Result<String> {
    let out = output(program, args)?;
    if !out.status.success() {
        bail!("{program} failed");
    }
    Ok(String::from_utf8(out.stdout)?
        .trim_end_matches(['\r', '\n'])
        .into())
}

pub fn code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

static ACTIVE_CHILD: Mutex<Option<libc::pid_t>> = Mutex::new(None);

// Keep the CLI in the foreground process group for terminal job control. Forward
// externally delivered termination signals too; the runtime handles container cleanup.
// Install once: removing a signal-hook handler would leave the signal ignored.
pub fn install_signals() -> Result<()> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    std::thread::spawn(move || {
        for signal in signals.forever() {
            let active = ACTIVE_CHILD.lock().unwrap();
            if let Some(pid) = *active {
                // SAFETY: the mutex prevents reaping this child while signalling it.
                unsafe {
                    libc::kill(pid, signal);
                }
            } else {
                std::process::exit(128 + signal);
            }
        }
    });
    Ok(())
}

/// Process-wide output policy, set once before any container work begins.
/// Like the signal handler above this is genuinely per-process: preparation
/// runs deep inside the backends, which have no terminal-facing API of their
/// own and must not grow one just to carry this flag.
static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(verbose: bool) {
    VERBOSE.store(verbose, Ordering::Relaxed);
}

/// When set, steps that are normally silent print instead, so a slow or failing
/// startup can be watched as it happens.
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn wait(command: &mut Command) -> Result<i32> {
    let mut active = ACTIVE_CHILD.lock().unwrap();
    let mut child = command.spawn().context("could not start child process")?;
    *active = Some(child.id() as libc::pid_t);
    drop(active);
    loop {
        let mut active = ACTIVE_CHILD.lock().unwrap();
        match child.try_wait() {
            Ok(Some(status)) => {
                *active = None;
                return Ok(code(status));
            }
            Err(error) => {
                *active = None;
                return Err(error.into());
            }
            Ok(None) => {}
        }
        drop(active);
        std::thread::sleep(Duration::from_millis(10));
    }
}
