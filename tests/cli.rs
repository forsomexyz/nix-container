#![cfg(unix)]
use std::{
    fs,
    io::Read,
    os::{fd::FromRawFd, unix::fs::PermissionsExt},
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for path in ["bin", "project", "home", "volumes", "networks"] {
            fs::create_dir(dir.path().join(path)).unwrap();
        }
        fs::write(dir.path().join("project/shell.nix"), "{}\n").unwrap();
        let f = Self { dir };
        f.script("docker", include_str!("mock-runtime.sh"));
        f
    }
    fn path(&self, path: &str) -> std::path::PathBuf {
        self.dir.path().join(path)
    }
    fn script(&self, name: &str, source: &str) {
        let path = self.path(&format!("bin/{name}"));
        fs::write(&path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nix-container"));
        c.current_dir(self.path("project"))
            .env("HOME", self.path("home"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.path("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("NIX_CONTAINER_CLI", self.path("bin/docker"))
            .env("MOCK_ROOT", self.dir.path())
            .env_remove("NIX_CONTAINER_MEMORY")
            .env_remove("NIX_CONTAINER_SHARED_NIX_EXPRS")
            .env_remove("GH_TOKEN")
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env_remove("AWS_PROFILE")
            .env_remove("PASSWORD")
            .stdin(Stdio::null());
        c
    }
    fn ok(&self, args: &[&str]) -> Output {
        let out = self.cmd().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn log(&self) -> String {
        fs::read_to_string(self.path("events")).unwrap_or_default()
    }
    fn argv(&self) -> Vec<String> {
        fs::read(self.path("argv"))
            .unwrap()
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect()
    }
}
fn pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|p| p == [flag, value])
}

#[test]
fn startup_is_quiet_without_a_terminal() {
    let f = Fixture::new();
    let out = f.ok(&[]);
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
    let out = f.ok(&["run", "true"]);
    assert!(out.stderr.is_empty());
}

fn with_stderr_pty(
    command: &mut Command,
    rows: u16,
    columns: u16,
) -> (std::process::ExitStatus, String) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty initializes both file descriptors on success.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: slave is a terminal descriptor; size contains its new dimensions.
    assert_eq!(unsafe { libc::ioctl(slave, libc::TIOCSWINSZ, &size) }, 0);
    // SAFETY: both file descriptors were returned by openpty and are owned here.
    let mut master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    let mut child = command
        .stderr(Stdio::from(slave))
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    // Command retains its Stdio handle after spawn; close it before reading the PTY.
    command.stderr(Stdio::null());
    let status = child.wait().unwrap();
    let mut output = String::new();
    // On Linux, reading the master after the last slave closes returns EIO.
    let _ = master.read_to_string(&mut output);
    (status, output)
}

#[test]
fn spinner_clears_before_container_output_or_error() {
    let f = Fixture::new();
    f.script("docker", "#!/bin/sh\nif [ \"$1\" = image ]; then sleep 0.4; exit 0; fi\nif [ \"$1\" = run ]; then echo shell-ready >&2; exit 0; fi\nexit 0\n");
    let (status, output) = with_stderr_pty(&mut f.cmd(), 24, 80);
    assert!(status.success(), "{output}");
    assert!(output.contains("N I X   C O N T A I N E R"), "{output:?}");
    assert!(output.contains("\x1b[38;5;213m◆"), "{output:?}");
    assert!(output.contains("starting nix container ..."), "{output:?}");
    assert!(output.contains("\x1b[1B\r\x1b[2K"), "{output:?}");
    assert!(output.contains("\x1b[4A\rshell-ready"), "{output:?}");

    f.script(
        "docker",
        "#!/bin/sh\nif [ \"$1\" = image ]; then sleep 0.4; exit 1; fi\nexit 0\n",
    );
    let (status, output) = with_stderr_pty(&mut f.cmd(), 24, 80);
    assert!(!status.success());
    assert!(output.contains("✦"), "{output:?}");
    assert!(
        output.contains("\x1b[4A\rnix-container: image"),
        "{output:?}"
    );

    let (status, output) = with_stderr_pty(&mut f.cmd().args(["run", "true"]), 24, 80);
    assert!(!status.success());
    assert!(!output.contains("✦"));

    let (status, output) = with_stderr_pty(&mut f.cmd(), 5, 30);
    assert!(!status.success());
    assert!(!output.contains("\x1b["), "{output:?}");
    assert!(output.contains("nix-container: image"));
}

#[test]
fn termination_signal_reaches_runtime() {
    let f = Fixture::new();
    f.script("docker", "#!/bin/sh\nif [ \"$1\" = image ]; then exit 0; fi\ntrap 'exit 143' TERM\ntouch \"$MOCK_ROOT/ready\"\nwhile :; do sleep 0.05; done\n");
    let mut child = f
        .cmd()
        .args(["run", "sleep", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !f.path("ready").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(f.path("ready").exists());
    // SAFETY: the child is alive and owned by this test.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(143));
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("termination signal was not forwarded");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn aws_cli_resets_config_profile_and_accepts_literal_boolean_profile() {
    let f = Fixture::new();
    f.script("aws", "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$MOCK_ROOT/aws-argv\"\nprintf 'AWS_ACCESS_KEY_ID=key\\n'\n");
    fs::write(f.path("project/nix-container.yml"), "with-aws: dev\n").unwrap();
    f.ok(&["--with-aws", "run", "true"]);
    assert!(!fs::read_to_string(f.path("aws-argv"))
        .unwrap()
        .contains("--profile"));
    f.ok(&["--with-aws=false", "run", "true"]);
    assert!(fs::read_to_string(f.path("aws-argv"))
        .unwrap()
        .starts_with("--profile\nfalse\n"));
}

#[test]
fn existing_network_regressions() {
    let out = Command::new("bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test-network.sh"))
        .env("NIX_CONTAINER_BIN", env!("CARGO_BIN_EXE_nix-container"))
        .env_remove("NIX_CONTAINER_MEMORY")
        .env_remove("NIX_CONTAINER_SHARED_NIX_EXPRS")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn config_precedence_mounts_ports_and_command_quoting() {
    let f = Fixture::new();
    fs::create_dir(f.path("data")).unwrap();
    fs::write(f.path("config.yml"), "memory: 2G\nports: [80, 90]\nmounts: data:/srv/data:ro\nwith-env: plain.env\nwith-shared-nix-exprs: []\n").unwrap();
    fs::write(f.path("plain.env"), "FOO=bar\n").unwrap();
    let out = f
        .cmd()
        .env("NIX_CONTAINER_MEMORY", "8G")
        .env("NIX_CONTAINER_SHARED_NIX_EXPRS", "bad=/missing")
        .args([
            "--config",
            "../config.yml",
            "-p",
            "0.0.0.0:3000:3000",
            "run",
            "printf",
            "%s",
            "a b",
            "'",
            "$(false)",
            "",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let args = f.argv();
    assert!(pair(&args, "--memory", "2G"));
    assert!(pair(&args, "-p", "127.0.0.1:3000:3000"));
    assert_eq!(args.iter().filter(|s| *s == "-p").count(), 1);
    assert!(pair(
        &args,
        "-v",
        &format!(
            "{}:/srv/data:ro",
            f.path("data").canonicalize().unwrap().display()
        )
    ));
    assert!(!args.iter().any(|s| s == "-t" || s == "-it"));
    let source = &args[args.iter().position(|s| s == "--run").unwrap() + 1];
    let result = Command::new("sh").args(["-c", source]).output().unwrap();
    assert_eq!(result.stdout, b"a b'$(false)");
}

#[test]
fn shared_expressions_and_npmrc_are_readonly_and_keep_baked_channel() {
    let f = Fixture::new();
    fs::create_dir(f.path("home/common")).unwrap();
    fs::write(f.path("home/.npmrc"), "registry=https://example.invalid\n").unwrap();
    f.ok(&[
        "--with-npmrc",
        "--with-shared-nix-exprs",
        "common=~/common",
        "run",
        "true",
    ]);
    let args = f.argv();
    assert!(pair(
        &args,
        "-v",
        &format!(
            "{}:/home/nix/.nix-shared-exprs/common:ro",
            f.path("home/common").canonicalize().unwrap().display()
        )
    ));
    assert!(pair(
        &args,
        "-v",
        &format!("{}:/home/nix/.npmrc:ro", f.path("home/.npmrc").display())
    ));
    assert!(pair(&args, "-e", "NIX_PATH=common=/home/nix/.nix-shared-exprs/common:/nix/var/nix/profiles/per-user/nix/channels:/home/nix/.nix-defexpr/channels"));
}

#[test]
fn invalid_mounts_and_persistence_never_reach_runtime() {
    let f = Fixture::new();
    for args in [
        vec!["--mount", "missing"],
        vec!["--mount", ".:/nix/store"],
        vec!["--mount", ".:/home/nix/../nix/.cache"],
        vec!["--persist", "/nix/store"],
        vec!["--persist", ".local", "--persist", ".local/share"],
        vec!["--mount", ".:/srv", "--persist", "/srv/db"],
    ] {
        let out = f.cmd().args(&args).args(["run", "true"]).output().unwrap();
        assert!(!out.status.success(), "accepted {args:?}");
    }
    assert!(f.log().is_empty());
}

#[test]
fn credentials_are_environment_only_and_op_wraps_launch() {
    let f = Fixture::new();
    f.script("gh", "#!/bin/sh\nprintf 'github-secret'\n");
    f.script("aws", "#!/bin/sh\nprintf 'AWS_ACCESS_KEY_ID=access-secret\\nAWS_SECRET_ACCESS_KEY=secret-key\\n'\n");
    f.script("op", "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexport PASSWORD=resolved-secret\nexec \"$@\"\n");
    fs::write(
        f.path("project/.env"),
        "PASSWORD=op://vault/item/password\n",
    )
    .unwrap();
    f.ok(&[
        "--with-gh-token",
        "--with-aws=dev",
        "--with-env",
        ".env",
        "run",
        "true",
    ]);
    let args = f.argv();
    for key in [
        "GH_TOKEN",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_PROFILE",
        "PASSWORD",
    ] {
        assert!(pair(&args, "-e", key));
    }
    for secret in [
        "github-secret",
        "access-secret",
        "secret-key",
        "resolved-secret",
    ] {
        assert!(!args.join(" ").contains(secret));
    }
    assert_eq!(
        fs::read_to_string(f.path("credentials")).unwrap(),
        "github-secret|access-secret|secret-key|dev|resolved-secret"
    );
}

#[test]
fn persistence_stays_attached_after_expression_change_and_cache_clear() {
    let f = Fixture::new();
    f.ok(&[
        "--persist",
        ".local/share/tool",
        "--persist",
        "/var/lib/db",
        "run",
        "true",
    ]);
    let first = f.argv();
    let persistent: Vec<_> = first
        .iter()
        .filter(|s| s.starts_with("nix-container-persist-"))
        .cloned()
        .collect();
    assert_eq!(persistent.len(), 2);
    assert!(persistent
        .iter()
        .any(|s| s.ends_with(":/home/nix/.nix-container-persist/.local/share/tool")));
    assert!(pair(&first, "--entrypoint", "sh"));
    assert!(pair(
        &first,
        "-e",
        "NIX_CONTAINER_PERSIST_LINKS=/home/nix/.local/share/tool"
    ));
    fs::write(f.path("project/shell.nix"), "{ changed = true; }\n").unwrap();
    f.ok(&[
        "--persist",
        ".local/share/tool",
        "--persist",
        "/var/lib/db",
        "run",
        "true",
    ]);
    for volume in persistent {
        assert!(f.argv().contains(&volume));
    }
    assert_eq!(f.log().matches("seed-persistence").count(), 2);
    f.ok(&["clear-cache", "--all"]);
    assert!(!f
        .log()
        .lines()
        .any(|l| l.starts_with("remove nix-container-persist-")));
}

#[test]
fn apple_preflight_seeding_and_stale_retry() {
    let f = Fixture::new();
    f.script("container", include_str!("mock-runtime.sh"));
    let out = f
        .cmd()
        .env("NIX_CONTAINER_CLI", f.path("bin/container"))
        .env("MOCK_STALE", "1")
        .args(["run", "false"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("stale cache detected"));
    assert_eq!(f.log().matches("seed-cache").count(), 2);
    let argv = f.argv();
    assert!(pair(&argv, "-e", "HOME=/home/nix"));
    assert!(argv.contains(&"--user".into()));
    // The probe must run only true, not the user command twice.
    assert!(f.log().contains("probe"));
    let out = f
        .cmd()
        .env("NIX_CONTAINER_CLI", f.path("bin/container"))
        .env("MOCK_STOPPED", "1")
        .args(["run", "true"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("container system start"));
}

#[test]
fn build_uses_embedded_assets_and_invalidates_only_caches() {
    for apple in [false, true] {
        let f = Fixture::new();
        f.script("container", include_str!("mock-runtime.sh"));
        for volume in [
            "nix-container-store-old",
            "nix-container-cache-old",
            "nix-container-persist-keep",
            "unrelated",
        ] {
            fs::write(f.path(&format!("volumes/{volume}")), "").unwrap();
        }
        let out = f
            .cmd()
            .env(
                "NIX_CONTAINER_CLI",
                f.path(if apple { "bin/container" } else { "bin/docker" }),
            )
            .args(["build", "--no-cache"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let log = f.log();
        assert!(log.contains("build --no-cache"));
        assert!(log.contains(if apple {
            "IMAGE_FORMAT=oci"
        } else {
            "IMAGE_FORMAT=docker"
        }));
        assert!(!f.path("volumes/nix-container-store-old").exists());
        assert!(f.path("volumes/nix-container-persist-keep").exists());
        assert!(f.path("volumes/unrelated").exists());
    }
}

#[test]
fn init_is_nondestructive_and_run_exit_status_propagates() {
    let f = Fixture::new();
    assert!(!f.cmd().arg("init").output().unwrap().status.success());
    assert_eq!(
        fs::read_to_string(f.path("project/shell.nix")).unwrap(),
        "{}\n"
    );
    f.ok(&["init", "--force"]);
    assert!(fs::read_to_string(f.path("project/shell.nix"))
        .unwrap()
        .contains("mkShellNoCC"));
    let out = f
        .cmd()
        .env("MOCK_EXIT", "42")
        .args(["run", "false"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(42));
    assert_eq!(f.log().matches("launch").count(), 2); // one command + diagnostic probe
}
