use crate::{
    backend::{
        self, BindMount, Environment, Identity, PersistenceVolume, RunInvocation, RunPlan, IMAGE,
    },
    options::{self, Invocation, Options},
    paths::{self, HOME, PERSIST_ROOT},
    runtime,
    secrets::{self, SecretHelper},
};
use anyhow::{bail, Context, Result};
use std::{
    collections::{BTreeMap, HashSet},
    env,
    fmt::Write as _,
    fs,
    io::{self, IsTerminal, Write},
    os::fd::AsRawFd,
    path::PathBuf,
    process::Stdio,
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const ANIMATION_HEIGHT: usize = 5;
const ANIMATION_MIN_COLUMNS: u16 = 40;
const STREAM_WIDTH: usize = 24;

fn animation_frame(frame: usize) -> String {
    const CYAN: &str = "\x1b[38;5;51m";
    const MAGENTA: &str = "\x1b[38;5;201m";
    const WHITE: &str = "\x1b[38;5;255m";
    const DIM: &str = "\x1b[38;5;30m";
    const RESET: &str = "\x1b[0m";
    const HEX: &[u8] = b"0123456789ABCDEF";

    // Single-column ASCII keeps the panel safe on the same small terminals as before.
    let mut screen = format!(
        "\r\x1b[2K  {CYAN}+[ {WHITE}NIX-CONTAINER {MAGENTA}// BOOT{CYAN} ]---------+{RESET}"
    );
    let _ = write!(screen, "\x1b[1B\r\x1b[2K  {CYAN}| {DIM}HEX // ");
    for column in 0..STREAM_WIDTH {
        let distance = (frame % STREAM_WIDTH + STREAM_WIDTH - column) % STREAM_WIDTH;
        let glitch = (frame % 37 + column * 3) % 37 == 0;
        let glyph = if glitch {
            '/'
        } else {
            HEX[(frame / 2 + column * 7) % HEX.len()] as char
        };
        let color = if glitch {
            MAGENTA
        } else {
            match distance {
                0 => WHITE,
                1..=4 => CYAN,
                _ => DIM,
            }
        };
        let _ = write!(screen, "{color}{glyph}");
    }
    let _ = write!(screen, "{CYAN}  |{RESET}");

    // A looping scanner, not a progress bar: dependency resolution has no fixed duration.
    let sweep = frame % (STREAM_WIDTH * 2 - 2);
    let head = sweep.min(STREAM_WIDTH * 2 - 2 - sweep);
    let _ = write!(screen, "\x1b[1B\r\x1b[2K  {CYAN}| {MAGENTA}LINK {CYAN}[");
    for column in 0..STREAM_WIDTH {
        let (glyph, color) = match column.abs_diff(head) {
            0 => ('#', WHITE),
            1 => ('=', MAGENTA),
            2 => ('-', MAGENTA),
            _ => ('.', DIM),
        };
        let _ = write!(screen, "{color}{glyph}");
    }
    let _ = write!(screen, "{CYAN}]  |{RESET}");
    let cursor = if (frame / 6) % 2 == 0 { '_' } else { ' ' };
    let _ = write!(
        screen,
        "\x1b[1B\r\x1b[2K  {CYAN}| {MAGENTA}>{cursor} {WHITE}initializing environment...{CYAN}   |{RESET}\
         \x1b[1B\r\x1b[2K  {CYAN}+----------------------------------+{RESET}\
         \x1b[{}A\r",
        ANIMATION_HEIGHT - 1
    );
    screen
}

// Only draw on a large enough terminal; never put cursor controls in captured output.
struct Spinner(Option<(Sender<()>, JoinHandle<()>)>);

impl Spinner {
    fn start(interactive: bool) -> Self {
        if !interactive || !io::stderr().is_terminal() {
            return Self(None);
        }
        // A narrow or short terminal would wrap the frame and make cleanup unsafe.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: size is writable and stderr is an open terminal descriptor.
        if unsafe { libc::ioctl(io::stderr().as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0
            || size.ws_col < ANIMATION_MIN_COLUMNS
            || size.ws_row < ANIMATION_HEIGHT as u16 + 1
        {
            return Self(None);
        }
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut stderr = io::stderr().lock();
            let mut frame = 0;
            // Delay the first frame so fast cached starts don't flash.
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(Duration::from_millis(90)) {
                if frame == 0 {
                    // Leave the caller's current line untouched. Even when the
                    // terminal scrolls, the first drawn row is below that line.
                    let _ = write!(
                        stderr,
                        "\r{}\x1b[{}A\r",
                        "\n".repeat(ANIMATION_HEIGHT),
                        ANIMATION_HEIGHT - 1
                    );
                }
                let _ = write!(stderr, "{}", animation_frame(frame));
                let _ = stderr.flush();
                frame += 1;
            }
            if frame > 0 {
                // Erase every reserved row and return to the first blank line.
                let mut clear = String::from("\r\x1b[0m");
                for row in 0..ANIMATION_HEIGHT {
                    clear.push_str("\x1b[2K");
                    if row + 1 < ANIMATION_HEIGHT {
                        clear.push_str("\x1b[1B\r");
                    }
                }
                let _ = write!(clear, "\x1b[{}A\r", ANIMATION_HEIGHT - 1);
                let _ = stderr.write_all(clear.as_bytes());
                let _ = stderr.flush();
            }
        });
        Self(Some((tx, handle)))
    }

    fn active(&self) -> bool {
        self.0.is_some()
    }

    fn stop(&mut self) {
        if let Some((tx, handle)) = self.0.take() {
            let _ = tx.send(());
            let _ = handle.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Plan {
    run: RunPlan,
    credentials: BTreeMap<String, String>,
    op_file: Option<String>,
}

impl Plan {
    fn new(options: &Options) -> Result<Self> {
        let hash = paths::project_hash()?;
        let project = env::current_dir()?.canonicalize()?;
        let name = project
            .file_name()
            .context("cannot run a project at the filesystem root")?
            .to_string_lossy();
        let project_dst = format!("{HOME}/{name}");
        let home = PathBuf::from(env::var_os("HOME").context("HOME is not set")?);
        if project.to_string_lossy().contains(':') {
            bail!("project path cannot contain ':'");
        }
        if ["/nix", "/home/nix/.cache", PERSIST_ROOT]
            .iter()
            .any(|p| paths::overlaps(p, &project_dst))
        {
            bail!("project directory conflicts with a managed mount: {project_dst}");
        }
        let mut binds = Vec::new();
        let mut environment = Vec::new();
        let mut env_file = None;
        let mut occupied = vec![
            "/nix".into(),
            "/home/nix/.cache".into(),
            project_dst.clone(),
            PERSIST_ROOT.into(),
        ];
        let mut credentials = BTreeMap::new();
        let mut op_file = None;
        // Parse references before launching any helper or touching the runtime.
        let secrets = options
            .list("secrets")
            .iter()
            .map(|reference| secrets::parse(reference))
            .collect::<Result<Vec<Box<dyn SecretHelper>>>>()?;
        let mut slots = HashSet::new();
        for secret in &secrets {
            if !slots.insert(secret.slot()) {
                bail!("secret is already provided: {}", secret.slot());
            }
        }
        if let Some(memory) = options.get("memory") {
            if !options::valid_memory(memory) {
                bail!("--memory: invalid memory size: {memory}");
            }
        }
        let ports = options
            .list("ports")
            .iter()
            .map(|p| paths::loopback(p))
            .collect::<Result<Vec<_>>>()?;
        let mut optional_mount = |source: PathBuf, target: &str, readonly: bool| -> Result<()> {
            if source.exists() {
                if source.to_string_lossy().contains(':') {
                    bail!("host path cannot contain ':'");
                }
                if occupied.iter().any(|p| paths::overlaps(p, target)) {
                    bail!("optional mount overlaps another mount: {target}");
                }
                binds.push(BindMount {
                    source,
                    target: target.into(),
                    readonly,
                });
                occupied.push(target.into());
            }
            Ok(())
        };
        for secret in &secrets {
            for file in secret.files(&home) {
                optional_mount(file.source, file.target, file.readonly)?;
            }
        }
        let mut seen = HashSet::new();
        let mut nix_path = Vec::new();
        for spec in options.list("with-shared-nix-exprs") {
            let (name, dir) = spec
                .split_once('=')
                .context("--with-shared-nix-exprs expects NAME=DIR")?;
            if !paths::valid_name(name) || matches!(name, "." | "..") {
                bail!("--with-shared-nix-exprs: invalid name: {name}");
            }
            if !seen.insert(name) {
                bail!("--with-shared-nix-exprs: duplicate name: {name}");
            }
            let dir = paths::host_path(dir, &home);
            if !dir.is_dir() {
                bail!(
                    "--with-shared-nix-exprs: not a directory: {}",
                    dir.display()
                );
            }
            let dir = dir.canonicalize()?;
            if dir.to_string_lossy().contains(':') {
                bail!("shared expression path cannot contain ':'");
            }
            let target = format!("{HOME}/.nix-shared-exprs/{name}");
            if occupied.iter().any(|p| paths::overlaps(p, &target)) {
                bail!("shared expression mount overlaps another mount: {target}");
            }
            binds.push(BindMount {
                source: dir,
                target: target.clone(),
                readonly: true,
            });
            nix_path.push(format!("{name}={target}"));
            occupied.push(target);
        }
        if !nix_path.is_empty() {
            environment.push(Environment::Value("NIX_PATH".into(), format!("{}:/nix/var/nix/profiles/per-user/nix/channels:/home/nix/.nix-defexpr/channels", nix_path.join(":"))));
        }
        for spec in options.list("mounts") {
            let mount = paths::mount(spec, &home)?;
            if let Some(other) = occupied.iter().find(|p| paths::overlaps(p, &mount.target)) {
                bail!(
                    "--mount: {} overlaps an existing or reserved container path ({other})",
                    mount.target
                );
            }
            occupied.push(mount.target.clone());
            binds.push(BindMount {
                source: mount.source,
                target: mount.target,
                readonly: mount.mode == "ro",
            });
        }
        let project_hash = paths::hash(project.as_os_str().as_encoded_bytes());
        let mut persistence = Vec::new();
        for spec in options.list("persistence") {
            let location = paths::persistence(spec, &home)?;
            if let Some(other) = occupied.iter().find(|p| paths::overlaps(p, &location)) {
                bail!("--persist: {location} overlaps another mount or a location managed by nix-container ({other})");
            }
            let volume = format!(
                "nix-container-persist-{project_hash}-{}",
                paths::hash(&location)
            );
            occupied.push(location.clone());
            persistence.push(PersistenceVolume {
                name: volume,
                location,
            });
        }
        // Resolve credentials only after validating the plan. Values are passed in the
        // child environment, never in argv and never exported into the caller's shell.
        for secret in &secrets {
            for (name, value) in secret.resolve()? {
                if credentials.contains_key(&name)
                    || environment
                        .iter()
                        .any(|item| matches!(item, Environment::Forward(other) if other == &name))
                {
                    bail!("secret variable is already forwarded: {name}");
                }
                credentials.insert(name.clone(), value);
                environment.push(Environment::Forward(name));
            }
        }
        for secret in &secrets {
            for name in secret.inherited_env() {
                if env::var_os(name).is_some() && !credentials.contains_key(*name) {
                    environment.push(Environment::Forward((*name).into()));
                }
            }
        }
        if let Some(file) = options.get("with-env") {
            let file = paths::host_path(file, &home);
            let text = fs::read_to_string(&file)
                .with_context(|| format!("env file not found or unreadable: {}", file.display()))?;
            if text.contains("op://") {
                if runtime::executable("op").is_none() {
                    bail!("{} has 1Password secret references (op://) but the 'op' CLI is not in PATH", file.display());
                }
                for line in text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                {
                    let line = line.strip_prefix("export ").unwrap_or(line);
                    if let Some((name, _)) = line.split_once('=') {
                        let name = name.trim();
                        if !paths::valid_env(name) {
                            bail!("invalid environment variable name in {}", file.display());
                        }
                        environment.push(Environment::Forward(name.into()));
                    }
                }
                op_file = Some(file.to_string_lossy().into());
            } else {
                env_file = Some(file);
            }
        }
        binds.push(BindMount {
            source: project.clone(),
            target: project_dst.clone(),
            readonly: false,
        });
        Ok(Self {
            run: RunPlan {
                image: IMAGE.into(),
                name: name.into(),
                identity: Identity::host(),
                home: HOME.into(),
                workdir: project_dst,
                binds,
                store: format!("nix-container-store-{hash}"),
                cache: format!("nix-container-cache-{hash}"),
                persistence,
                memory: options.get("memory").map(str::to_owned),
                network: options.get("network").map(str::to_owned),
                ports,
                env: environment,
                env_file,
            },
            credentials,
            op_file,
        })
    }
}

/// Build the project's shell derivation with an emptied `shellHook`.
///
/// nix-shell runs `shellHook` even under `--run`, so warming the dependency
/// closure with a plain `--run true` would run the user's hook twice per
/// session. Overriding the attribute changes the shell derivation's own hash
/// but not its `inputDrvs`, which is what actually gets built, so the warm-up
/// realizes exactly the closure the real session then reuses from the shared
/// store volume. Both guards matter: the file may evaluate to a derivation
/// rather than a function, and a bare `derivation {}` has no `overrideAttrs`.
fn warm_expression(file: &str) -> String {
    format!(
        "let f = import ./{file}; \
           d = if builtins.isFunction f then f {{}} else f; \
         in if d ? overrideAttrs then d.overrideAttrs (_: {{ shellHook = \"\"; }}) else d"
    )
}

/// Realize the dependency closure without opening a shell, so the animation can
/// cover the slow part of startup. Never a TTY: under --verbose nix then emits
/// plain lines rather than redrawing a progress display over the session's own
/// output.
fn warm_invocation(file: &str) -> RunInvocation {
    RunInvocation {
        args: vec![
            "--extra-experimental-features".into(),
            "flakes".into(),
            "-E".into(),
            warm_expression(file),
            "--run".into(),
            "true".into(),
        ],
        attach_stdin: false,
        tty: false,
    }
}

fn shell_invocation(command: Option<&[String]>, terminal: bool) -> RunInvocation {
    let mut args = vec!["--extra-experimental-features".into(), "flakes".into()];
    if let Some(command) = command {
        args.push("--run".into());
        args.push(
            command
                .iter()
                .map(|s| paths::quote(s))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    RunInvocation {
        args,
        attach_stdin: terminal && (command.is_none() || io::stdin().is_terminal()),
        tty: terminal
            && (command.is_none() || (io::stdin().is_terminal() && io::stdout().is_terminal())),
    }
}

pub fn run(invocation: Invocation) -> Result<i32> {
    runtime::set_verbose(invocation.verbose);
    // --verbose replaces the animation with the output it would have hidden.
    let mut spinner = Spinner::start(invocation.command.is_none() && !invocation.verbose);
    let options = options::load(&invocation)?;
    let plan = Plan::new(&options)?;
    let backend = backend::detect()?;
    backend.preflight()?;
    if backend.image_id(IMAGE)?.is_none() {
        bail!("image {IMAGE} not found. Run 'nix-container build' to build it.");
    }
    let prepared = backend.prepare_run(&plan.run)?;
    // Keep the animation up through dependency resolution, so it ends when the
    // shell is actually ready rather than when the container is merely started.
    // A cold build can take minutes and stays behind the animation; --verbose
    // is the way to watch it. Worth a second container only when there is an
    // animation to cover or output to show; a failure here is not reported,
    // since the real session reports it in context.
    if spinner.active() || invocation.verbose {
        if let Ok(file) = paths::shell_file() {
            if let Ok(warm) = backend.command(&prepared, &warm_invocation(file)) {
                let mut command = warm.launch(&plan.credentials, plan.op_file.as_deref());
                command.stdin(Stdio::null());
                if !invocation.verbose {
                    // Nothing may reach the terminal while a frame is drawn.
                    command.stdout(Stdio::null()).stderr(Stdio::null());
                }
                let _ = runtime::wait(&mut command);
            }
        }
    }
    spinner.stop();
    let session = shell_invocation(invocation.command.as_deref(), true);
    let command = backend.command(&prepared, &session)?;
    let start = Instant::now();
    let code = runtime::wait(&mut command.launch(&plan.credentials, plan.op_file.as_deref()))?;
    if code != 0 && code < 128 && start.elapsed() < Duration::from_secs(3) {
        // Probe nix-shell only, never repeat the user's command during diagnosis.
        let probe = shell_invocation(Some(&["true".into()]), false);
        let output = backend
            .command(&prepared, &probe)?
            .launch(&plan.credentials, plan.op_file.as_deref())
            .stdin(Stdio::null())
            .output()?;
        if [output.stdout, output.stderr]
            .concat()
            .windows(b"cannot determine user's home directory".len())
            .any(|w| w == b"cannot determine user's home directory")
        {
            eprintln!(
                "nix-container: stale cache detected for this project, clearing and retrying..."
            );
            for volume in [&plan.run.store, &plan.run.cache] {
                if let Err(error) = backend.remove_volume(volume) {
                    eprintln!("nix-container: cannot clear cache: {error:#}; exit other sessions and re-run");
                    return Ok(code);
                }
            }
            let prepared = backend.prepare_run(&plan.run)?;
            return runtime::wait(
                &mut backend
                    .command(&prepared, &session)?
                    .launch(&plan.credentials, plan.op_file.as_deref()),
            );
        }
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible_rows(screen: &str) -> Vec<String> {
        screen
            .split("\x1b[1B\r")
            .map(|row| {
                let mut visible = String::new();
                let mut chars = row.chars();
                while let Some(ch) = chars.next() {
                    if ch == '\x1b' {
                        assert_eq!(chars.next(), Some('['));
                        for control in chars.by_ref() {
                            if control.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    } else if ch != '\r' {
                        visible.push(ch);
                    }
                }
                visible
            })
            .collect()
    }

    #[test]
    fn animation_frames_fit_reserved_rows_and_reset_the_cursor() {
        for frame in (0..512).chain([usize::MAX]) {
            let screen = animation_frame(frame);
            assert_eq!(screen.matches("\x1b[2K").count(), ANIMATION_HEIGHT);
            assert!(screen.ends_with(&format!("\x1b[0m\x1b[{}A\r", ANIMATION_HEIGHT - 1)));
            let rows = visible_rows(&screen);
            assert_eq!(rows.len(), ANIMATION_HEIGHT);
            for row in &rows {
                assert!(row.is_ascii(), "single-column glyphs: {row:?}");
                assert_eq!(row.len(), 38, "fixed panel width: {row:?}");
                assert!(row.len() < usize::from(ANIMATION_MIN_COLUMNS));
            }
            assert!(rows[0].contains("NIX-CONTAINER // BOOT"));
            assert!(rows[3].contains("initializing environment..."));
        }
    }

    #[test]
    fn animation_scanner_bounces_while_data_and_cursor_animate() {
        let first = visible_rows(&animation_frame(0));
        let next = visible_rows(&animation_frame(1));
        let edge = visible_rows(&animation_frame(STREAM_WIDTH - 1));
        let back = visible_rows(&animation_frame(STREAM_WIDTH));
        let looped = visible_rows(&animation_frame(STREAM_WIDTH * 2 - 2));
        let head = |rows: &[String]| rows[2].find('#').unwrap();
        assert_eq!(head(&next), head(&first) + 1);
        assert_eq!(head(&edge), head(&first) + STREAM_WIDTH - 1);
        assert_eq!(head(&back), head(&edge) - 1);
        assert_eq!(first[2], looped[2]);
        assert_ne!(first[1], next[1], "hex data changes between frames");
        assert!(first[3].contains(">_"));
        assert!(visible_rows(&animation_frame(6))[3].contains("> "));
        assert_eq!(first[0], next[0], "branding stays readable");
    }
}
