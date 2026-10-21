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
        f.script("container", include_str!("mock-runtime.sh"));
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
        self.read_argv("argv")
    }
    fn read_argv(&self, name: &str) -> Vec<String> {
        fs::read(self.path(name))
            .unwrap()
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect()
    }
    fn runs(&self) -> Vec<Vec<String>> {
        (0..)
            .map(|n| format!("run-argv-{n}"))
            .take_while(|name| self.path(name).exists())
            .map(|name| self.read_argv(&name))
            .collect()
    }
    fn backend(&self, cli: &str) -> Command {
        let mut command = self.cmd();
        command.env("NIX_CONTAINER_CLI", self.path(&format!("bin/{cli}")));
        command
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
    f.script("docker", "#!/bin/sh\nif [ \"$1\" = image ]; then sleep 0.4; echo image-id; exit 0; fi\nif [ \"$1\" = run ]; then echo shell-ready >&2; exit 0; fi\nexit 0\n");
    let (status, output) = with_stderr_pty(&mut f.cmd(), 24, 80);
    assert!(status.success(), "{output}");
    assert!(output.contains("NIX-CONTAINER"), "{output:?}");
    assert!(output.contains("\x1b[38;5;201m// BOOT"), "{output:?}");
    assert!(output.contains("\x1b[38;5;51m+["), "{output:?}");
    assert!(output.contains("HEX //"), "{output:?}");
    assert!(output.contains("LINK"), "{output:?}");
    assert!(output.contains("initializing environment..."), "{output:?}");
    assert!(output.contains("\x1b[1B\r\x1b[2K"), "{output:?}");
    assert!(output.contains("\x1b[4A\rshell-ready"), "{output:?}");

    f.script(
        "docker",
        "#!/bin/sh\nif [ \"$1\" = image ]; then sleep 0.4; echo 'No such image' >&2; exit 1; fi\nexit 0\n",
    );
    let (status, output) = with_stderr_pty(&mut f.cmd(), 24, 80);
    assert!(!status.success());
    assert!(output.contains("NIX-CONTAINER"), "{output:?}");
    assert!(
        output.contains("\x1b[4A\rnix-container: image"),
        "{output:?}"
    );

    let (status, output) = with_stderr_pty(f.cmd().args(["run", "true"]), 24, 80);
    assert!(!status.success());
    assert!(!output.contains("\x1b["), "{output:?}");

    for (rows, columns) in [(5, 80), (24, 39)] {
        let (status, output) = with_stderr_pty(&mut f.cmd(), rows, columns);
        assert!(!status.success());
        assert!(!output.contains("\x1b["), "{output:?}");
        assert!(output.contains("nix-container: image"));
    }
}

#[test]
fn spinner_fits_the_smallest_supported_terminal() {
    let f = Fixture::new();
    f.script("docker", "#!/bin/sh\nif [ \"$1\" = image ]; then sleep 0.4; echo image-id; exit 0; fi\nif [ \"$1\" = run ]; then echo shell-ready >&2; exit 0; fi\nexit 0\n");
    let (status, output) = with_stderr_pty(&mut f.cmd(), 6, 40);
    assert!(status.success(), "{output}");
    assert!(output.contains("NIX-CONTAINER"), "{output:?}");
    assert!(output.contains("\x1b[4A\rshell-ready"), "{output:?}");
}

#[test]
fn warm_up_resolves_dependencies_without_running_the_shell_hook() {
    // The default mock runtime is silent on `run` and records every launch.
    let f = Fixture::new();
    let (status, _) = with_stderr_pty(&mut f.cmd(), 24, 80);
    assert!(status.success());
    let runs = f.runs();
    assert_eq!(runs.len(), 2, "warm-up then session: {runs:?}");

    let (warm, session) = (&runs[0], &runs[1]);
    // The warm-up resolves the closure without a shell and without the hook.
    let expression = warm.iter().position(|a| a == "-E").map(|i| &warm[i + 1]);
    let expression = expression.expect("warm-up passes an expression");
    assert!(expression.contains("shellHook = \"\""), "{expression}");
    assert!(expression.contains("import ./shell.nix"), "{expression}");
    assert!(expression.contains("builtins.isFunction"), "{expression}");
    assert!(expression.contains("overrideAttrs"), "{expression}");
    assert!(warm.windows(2).any(|p| p == ["--run", "true"]), "{warm:?}");
    // Never a TTY, or nix would draw over the frame with control sequences.
    assert!(!warm.iter().any(|a| a == "-it" || a == "-t"), "{warm:?}");

    // The real session is untouched: interactive, no expression override.
    assert!(session.iter().any(|a| a == "-it"), "{session:?}");
    assert!(!session.iter().any(|a| a == "-E"), "{session:?}");
    assert!(
        !session.iter().any(|a| a.contains("shellHook")),
        "{session:?}"
    );

    // Without a terminal there is no animation to cover, so no second container.
    let f = Fixture::new();
    f.ok(&[]);
    assert_eq!(f.runs().len(), 1, "no warm-up without a terminal");
}

#[test]
fn verbose_shows_startup_output_instead_of_the_animation() {
    // Distinguish the two launches: only the warm-up carries an expression.
    // The sleep outlasts the animation's startup delay, so a frame is drawn.
    let chatty = "#!/bin/sh\nif [ \"$1\" = image ]; then sleep 0.4; echo image-id; exit 0; fi\nif [ \"$1\" = run ]; then\n  case \"$*\" in *-E*) echo building-dependencies >&2 ;; *) echo shell-ready >&2 ;; esac\n  exit 0\nfi\nexit 0\n";

    // A cold build stays behind the animation: the frame survives to the session.
    let f = Fixture::new();
    f.script("docker", chatty);
    let (status, output) = with_stderr_pty(&mut f.cmd(), 24, 80);
    assert!(status.success(), "{output}");
    assert!(output.contains("NIX-CONTAINER"), "{output:?}");
    assert!(!output.contains("building-dependencies"), "{output:?}");
    assert!(output.contains("\x1b[4A\rshell-ready"), "{output:?}");

    // --verbose trades the animation for everything it would have hidden.
    let f = Fixture::new();
    f.script("docker", chatty);
    let (status, output) = with_stderr_pty(f.cmd().arg("--verbose"), 24, 80);
    assert!(status.success(), "{output}");
    assert!(output.contains("building-dependencies"), "{output:?}");
    assert!(output.contains("shell-ready"), "{output:?}");
    assert!(!output.contains("\x1b["), "no animation: {output:?}");

    // The warm-up still runs under --verbose: debugging must see the real path.
    let f = Fixture::new();
    let (status, _) = with_stderr_pty(f.cmd().arg("-v"), 24, 80);
    assert!(status.success());
    let runs = f.runs();
    assert_eq!(runs.len(), 2, "warm-up then session: {runs:?}");
    assert!(runs[0].iter().any(|a| a == "-E"), "{runs:?}");
}

#[test]
fn termination_signal_reaches_runtime() {
    let f = Fixture::new();
    f.script("docker", "#!/bin/sh\nif [ \"$1\" = image ]; then echo image-id; exit 0; fi\ntrap 'exit 143' TERM\ntouch \"$MOCK_ROOT/ready\"\nwhile :; do sleep 0.05; done\n");
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
fn aws_secret_reference_selects_profile_and_cli_replaces_config_list() {
    let f = Fixture::new();
    f.script("aws", "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$MOCK_ROOT/aws-argv\"\nprintf 'AWS_ACCESS_KEY_ID=key\\n'\n");
    fs::write(
        f.path("project/nix-container.yml"),
        "secrets: aws://profile/dev\n",
    )
    .unwrap();
    f.ok(&["run", "true"]);
    assert!(fs::read_to_string(f.path("aws-argv"))
        .unwrap()
        .starts_with("--profile\ndev\n"));
    f.ok(&["--secret", "aws://default", "run", "true"]);
    assert!(!fs::read_to_string(f.path("aws-argv"))
        .unwrap()
        .contains("--profile"));
    f.ok(&["--secret", "aws://profile/false", "run", "true"]);
    assert!(fs::read_to_string(f.path("aws-argv"))
        .unwrap()
        .starts_with("--profile\nfalse\n"));
}

#[test]
fn retired_secret_options_are_rejected() {
    for flag in [
        "--with-gh-token",
        "--with-npmrc",
        "--with-aws",
        "--with-aws=dev",
    ] {
        let f = Fixture::new();
        let out = f.cmd().args([flag, "run", "true"]).output().unwrap();
        assert!(!out.status.success(), "accepted {flag}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("unknown option"));
        assert!(f.log().is_empty());
    }
    for key in ["with-gh-token", "with-npmrc", "with-aws", "aws-profile"] {
        let f = Fixture::new();
        fs::write(
            f.path("project/nix-container.yml"),
            format!("{key}: true\n"),
        )
        .unwrap();
        let out = f.cmd().args(["run", "true"]).output().unwrap();
        assert!(!out.status.success(), "accepted {key}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("unknown option"));
        assert!(f.log().is_empty());
    }
}

#[test]
fn existing_network_regressions() {
    for cli in ["mock-container", "container"] {
        let out = Command::new("bash")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test-network.sh"))
            .env("NIX_CONTAINER_BIN", env!("CARGO_BIN_EXE_nix-container"))
            .env("MOCK_CLI_NAME", cli)
            .env_remove("NIX_CONTAINER_MEMORY")
            .env_remove("NIX_CONTAINER_SHARED_NIX_EXPRS")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{cli}: stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
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
        "--secret",
        "npmrc://default",
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
    for cli in ["docker", "container"] {
        let out = f
            .backend(cli)
            .args([
                "--secret",
                "gh://github.com/alice",
                "--secret",
                "aws://profile/dev",
                "--with-env",
                ".env",
                "run",
                "true",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
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
        for run in f.runs() {
            for secret in [
                "github-secret",
                "access-secret",
                "secret-key",
                "resolved-secret",
            ] {
                assert!(!run.join(" ").contains(secret));
            }
        }
        assert_eq!(
            fs::read_to_string(f.path("credentials")).unwrap(),
            "github-secret|access-secret|secret-key|dev|resolved-secret"
        );
    }
}

#[test]
fn secret_references_deliver_env_and_files_without_runtime_argv_values() {
    for cli in ["docker", "container"] {
        let f = Fixture::new();
        f.script("gh", "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$MOCK_ROOT/gh-argv\"\nprintf 'github-secret\\n'\n");
        f.script("aws", "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$MOCK_ROOT/aws-argv\"\nprintf 'AWS_ACCESS_KEY_ID=aws-secret\\n'\n");
        fs::write(
            f.path("home/.npmrc"),
            "//registry.example/:_authToken=npm-secret\n",
        )
        .unwrap();
        fs::write(
            f.path("project/nix-container.yml"),
            "secrets: [gh://github.com/alice, npmrc://default]\n",
        )
        .unwrap();
        let out = f
            .backend(cli)
            .args([
                "--secret=gh://github.com/alice",
                "--secret=gh://github.com/bob",
                "run",
                "true",
            ])
            .output()
            .unwrap();
        assert!(!out.status.success()); // Both references deliver GH_TOKEN.
        assert!(f.log().is_empty()); // Collision detected before any helper or runtime runs.
                                     // Use config for GH + npmrc; CLI supplies a different secret list on the next run.
        let out = f.backend(cli).args(["run", "true"]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fs::read_to_string(f.path("gh-argv")).unwrap(),
            "auth\ntoken\n--hostname\ngithub.com\n--user\nalice\n"
        );
        assert!(pair(&f.argv(), "-e", "GH_TOKEN"));
        assert!(pair(
            &f.argv(),
            "-v",
            &format!("{}:/home/nix/.npmrc:ro", f.path("home/.npmrc").display())
        ));
        assert_eq!(
            fs::read_to_string(f.path("credentials")).unwrap(),
            "github-secret||||"
        );
        for run in f.runs() {
            assert!(!run.join(" ").contains("github-secret"));
        }

        let out = f
            .backend(cli)
            .args(["--secret", "aws://profile/dev", "run", "true"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fs::read_to_string(f.path("aws-argv")).unwrap(),
            "--profile\ndev\nconfigure\nexport-credentials\n--format\nenv-no-export\n"
        );
        assert!(pair(&f.argv(), "-e", "AWS_ACCESS_KEY_ID"));
        assert_eq!(
            fs::read_to_string(f.path("credentials")).unwrap(),
            "|aws-secret||dev|"
        );
        assert!(!f.argv().join(" ").contains("aws-secret"));
        assert!(!f.argv().iter().any(|s| s.ends_with(":/home/nix/.npmrc:ro")));

        let out = f
            .backend(cli)
            .args([
                "--secret",
                "gh://github.com/alice",
                "--secret",
                "aws://default",
                "run",
                "true",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fs::read_to_string(f.path("credentials")).unwrap(),
            "github-secret|aws-secret|||"
        );
        for name in ["GH_TOKEN", "AWS_ACCESS_KEY_ID"] {
            assert!(pair(&f.argv(), "-e", name));
        }
    }
}

#[test]
fn invalid_secret_references_and_collisions_do_not_launch() {
    for spec in [
        "gh://github.com",
        "gh://github.com/alice/extra",
        "gh://-h/alice",
        "npmrc://unknown",
        "aws://profile/-bad",
        "unknown://x",
    ] {
        let f = Fixture::new();
        let out = f
            .cmd()
            .args(["--secret", spec, "run", "true"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "accepted {spec}");
        assert!(f.log().is_empty());
    }
    let f = Fixture::new();
    fs::write(f.path("home/.npmrc"), "secret").unwrap();
    for args in [
        vec!["--secret", "npmrc://default", "--secret", "npmrc://default"],
        vec![
            "--secret",
            "gh://github.com/alice",
            "--secret",
            "gh://github.com/bob",
        ],
        vec!["--secret", "aws://default", "--secret", "aws://profile/dev"],
        vec![
            "--secret",
            "npmrc://default",
            "--mount",
            ".:/home/nix/.npmrc",
        ],
    ] {
        let out = f.cmd().args(args).args(["run", "true"]).output().unwrap();
        assert!(!out.status.success());
        assert!(!f.log().contains("launch"));
    }
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
        let load = log.lines().find(|line| line.starts_with("load ")).unwrap();
        assert!(load.ends_with(if apple {
            "image-oci.tar"
        } else {
            "image.tar.gz"
        }));
        let builder = &f.runs()[0];
        assert!(!builder.contains(&"--user".into()));
        assert!(!pair(builder, "-e", "HOME=/home/nix"));
        assert!(!f.path("volumes/nix-container-store-old").exists());
        assert!(f.path("volumes/nix-container-persist-keep").exists());
        assert!(f.path("volumes/unrelated").exists());
    }
}

#[test]
fn build_retains_unchanged_or_in_use_caches() {
    for cli in ["docker", "container"] {
        for unchanged in [false, true] {
            let f = Fixture::new();
            fs::write(f.path("volumes/nix-container-store-old"), "cache").unwrap();
            if unchanged {
                fs::write(f.path("loaded"), "").unwrap();
            }
            let out = f
                .backend(cli)
                .env("MOCK_REMOVE_FAIL", if unchanged { "0" } else { "1" })
                .arg("build")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                fs::read_to_string(f.path("volumes/nix-container-store-old")).unwrap(),
                "cache"
            );
            if unchanged {
                assert!(!String::from_utf8_lossy(&out.stdout).contains("invalidated"));
            } else {
                assert!(String::from_utf8_lossy(&out.stderr).contains("volume is in use"));
            }
        }
    }
}

#[test]
fn backends_prepare_persistence_and_keep_helper_identity_separate() {
    for cli in ["docker", "container"] {
        let f = Fixture::new();
        for _ in 0..2 {
            let out = f
                .backend(cli)
                .args([
                    "--persist",
                    ".local/share/tool",
                    "--persist",
                    "/var/lib/db",
                    "run",
                    "true",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert_eq!(f.log().matches("seed-persistence").count(), 2);
        assert_eq!(
            f.log().matches("seed-cache").count(),
            usize::from(cli == "container")
        );
        let args = f.argv();
        assert!(pair(&args, "--entrypoint", "sh"));
        assert!(pair(
            &args,
            "-e",
            "NIX_CONTAINER_PERSIST_LINKS=/home/nix/.local/share/tool"
        ));
        assert!(args
            .iter()
            .any(|s| s.ends_with(":/home/nix/.nix-container-persist/.local/share/tool")));
        assert!(args.iter().any(|s| s.ends_with(":/var/lib/db")));
        assert_eq!(args.contains(&"--user".into()), cli == "container");
        assert_eq!(pair(&args, "-e", "HOME=/home/nix"), cli == "container");
        for helper in f
            .runs()
            .iter()
            .filter(|args| args.iter().any(|s| s.contains(":/seed-")))
        {
            assert!(pair(helper, "--user", "0"));
            assert!(pair(helper, "--entrypoint", "sh"));
            assert!(!pair(helper, "-e", "HOME=/home/nix"));
            assert!(!helper
                .iter()
                .any(|s| matches!(s.as_str(), "-i" | "-t" | "-it")));
            if helper.iter().any(|s| s.ends_with(":/seed-persistence")) {
                assert!(
                    helper.contains(&"/home/nix/.local/share/tool".into())
                        || helper.contains(&"/var/lib/db".into())
                );
            } else {
                assert!(helper.iter().any(|s| s.ends_with(":/seed-nix")));
                assert!(helper.iter().any(|s| s.ends_with(":/seed-cache")));
                assert!(!helper.iter().any(|s| s.ends_with(":/nix")));
            }
        }
    }
}

#[test]
fn apple_reseeds_when_either_cache_is_missing() {
    for prefix in ["nix-container-store-", "nix-container-cache-"] {
        let f = Fixture::new();
        assert!(f
            .backend("container")
            .args(["run", "true"])
            .output()
            .unwrap()
            .status
            .success());
        let volume = fs::read_dir(f.path("volumes"))
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.file_name().to_string_lossy().starts_with(prefix))
            .unwrap();
        fs::remove_file(volume.path()).unwrap();
        assert!(f
            .backend("container")
            .args(["run", "true"])
            .output()
            .unwrap()
            .status
            .success());
        assert!(volume.path().exists());
        assert_eq!(f.log().matches("seed-cache").count(), 2);
    }
}

#[test]
fn failed_preparation_never_launches_session_or_deletes_persistence() {
    for cli in ["docker", "container"] {
        let f = Fixture::new();
        // Prepare caches first so a subsequent failure is in persistence setup.
        assert!(f
            .backend(cli)
            .args(["run", "true"])
            .output()
            .unwrap()
            .status
            .success());
        let launches = f.log().matches("launch").count();
        let out = f
            .backend(cli)
            .env("MOCK_SEED_FAIL", "1")
            .args(["--persist", ".local/share/tool", "run", "true"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("volume retained"));
        assert_eq!(f.log().matches("launch").count(), launches);
        let volume = fs::read_dir(f.path("volumes"))
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("nix-container-persist-")
            })
            .unwrap();
        fs::write(volume.path(), "user data").unwrap();
        assert!(f
            .backend(cli)
            .args(["clear-cache", "--all"])
            .output()
            .unwrap()
            .status
            .success());
        assert_eq!(fs::read_to_string(volume.path()).unwrap(), "user data");
    }
    let f = Fixture::new();
    let out = f
        .backend("container")
        .env("MOCK_SEED_FAIL", "1")
        .args(["run", "true"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed to prepare the /nix"));
    assert!(!f.log().contains("launch"));
}

#[test]
fn both_backends_probe_safely_and_retry_only_after_successful_cache_removal() {
    for cli in ["docker", "container"] {
        for remove_fails in [false, true] {
            let f = Fixture::new();
            let out = f
                .backend(cli)
                .env("MOCK_STALE", "1")
                .env("MOCK_REMOVE_FAIL", if remove_fails { "1" } else { "0" })
                .args(["--persist", ".local/share/tool", "run", "user-command"])
                .output()
                .unwrap();
            assert_eq!(
                out.status.success(),
                !remove_fails,
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let runs = f.runs();
            let sessions: Vec<_> = runs
                .iter()
                .filter(|args| args.contains(&"--name".into()))
                .collect();
            assert_eq!(sessions.len(), if remove_fails { 2 } else { 3 });
            assert_eq!(sessions[0].last().unwrap(), "'user-command'");
            assert_eq!(sessions[1].last().unwrap(), "'true'");
            assert!(!sessions[1]
                .iter()
                .any(|s| matches!(s.as_str(), "-i" | "-t" | "-it")));
            assert!(pair(sessions[1], "--entrypoint", "sh"));
            assert_eq!(
                pair(sessions[1], "-e", "HOME=/home/nix"),
                cli == "container"
            );
            if !remove_fails {
                assert_eq!(sessions[2].last().unwrap(), "'user-command'");
            }
            assert_eq!(f.log().matches("seed-persistence").count(), 1);
            assert_eq!(
                f.log().matches("seed-cache").count(),
                if cli == "container" {
                    if remove_fails {
                        1
                    } else {
                        2
                    }
                } else {
                    0
                }
            );
            assert!(!f
                .log()
                .lines()
                .any(|l| l.starts_with("remove nix-container-persist-")));
        }
    }
}

#[test]
fn missing_images_and_runtime_failures_are_distinct() {
    for cli in ["docker", "container"] {
        for (mode, message) in [
            ("missing", "Run 'nix-container build'"),
            ("error", "cannot connect to runtime"),
            ("service", "image service not found"),
        ] {
            let f = Fixture::new();
            let out = f
                .backend(cli)
                .env("MOCK_IMAGE", mode)
                .args(["run", "true"])
                .output()
                .unwrap();
            assert!(!out.status.success());
            let error = String::from_utf8_lossy(&out.stderr);
            assert!(error.contains(message), "{error}");
            if mode != "missing" {
                assert!(!error.contains("Run 'nix-container build'"));
            }
            assert!(f.runs().is_empty());
        }
    }
    let f = Fixture::new();
    let out = f
        .backend("container")
        .env("MOCK_IMAGE", "malformed")
        .args(["run", "true"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("returned no ID"));
}

#[test]
fn clear_cache_needs_no_guest_kernel_and_propagates_removal_errors() {
    for cli in ["docker", "container"] {
        let f = Fixture::new();
        let out = f
            .backend(cli)
            .env("MOCK_STOPPED", "1")
            .env("MOCK_NO_KERNEL", "1")
            .arg("clear-cache")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("does not exist, skipping"));
        fs::write(f.path("volumes/nix-container-store-old"), "cache").unwrap();
        let out = f
            .backend(cli)
            .env("MOCK_REMOVE_FAIL", "1")
            .args(["clear-cache", "--all"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("volume is in use"));
        assert!(f.path("volumes/nix-container-store-old").exists());
        let out = f
            .backend(cli)
            .env("MOCK_VOLUME_ERROR", "1")
            .args(["clear-cache", "--all"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("permission denied"));
    }
    let f = Fixture::new();
    let out = f
        .backend("container")
        .env("MOCK_NO_KERNEL", "1")
        .args(["run", "true"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("container system kernel set --recommended")
    );
    assert!(f.runs().is_empty());
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
