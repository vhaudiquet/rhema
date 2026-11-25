//! The suite, live-gated: every machine test asks `ecrou_ready` first.

use super::*;
use crate::cli::{scratch_dir, shell_line, snap_common};
use crate::gitenv::{GIT_GUARDS, render_pairs};
use crate::tail::append_capped;
use rhema_sandbox::OutputOnly;
use rhema_sandbox::exec_util::EXEC_TAIL_BYTES;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Whether the host answers a live-machine test: `ecrou probe` green,
/// a current binary (the `--capture` flag exists), an alpine tarball
/// already in the cache, the tests run no downloads, and one smoke
/// boot with a shared tempdir, the exact posture the tests use. The
/// answer is cached: the smoke boot is a real machine, and the four
/// live tests want it once.
#[cfg(all(test, unix))]
fn ecrou_ready() -> bool {
    static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *READY.get_or_init(|| {
        let answers = |args: &[&str]| {
            std::process::Command::new("ecrou")
                .args(args)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .stdin(std::process::Stdio::null())
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
        };
        if !answers(&["probe"]) {
            return false;
        }
        let help = std::process::Command::new("ecrou")
            .args(["run", "--help"])
            .stdin(std::process::Stdio::null())
            .output();
        let current = help
            .map(|out| String::from_utf8_lossy(&out.stdout).contains("--capture"))
            .unwrap_or(false);
        if !current {
            return false;
        }
        let Ok(cache) = std::env::var("HOME") else {
            return false;
        };
        let cache = Path::new(&cache).join(".cache").join("ecrou");
        let cached = std::fs::read_dir(&cache)
            .map(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with("alpine") && name.ends_with(".tar.gz")
                })
            })
            .unwrap_or(false);
        if !cached {
            return false;
        }
        // The smoke boot: one machine, a tempdir share, the whole
        // path a live test takes, minus the assertions.
        let Ok(smoke) = scratch_dir() else {
            return false;
        };
        let share = smoke.path().to_string_lossy().into_owned();
        std::process::Command::new("ecrou")
            .args([
                "run",
                "--quiet",
                "--capture",
                smoke.path().join("io").to_str().unwrap_or_default(),
                "-v",
                &format!("{share}:{share}"),
                "--workdir",
                &share,
                "alpine:3.23",
                "--",
                "/bin/sh",
                "-c",
                "true",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// The snap detector: snapd's launcher under its aliases counts, a
/// native build never does.
#[test]
fn the_capture_scratch_only_moves_for_the_snap() {
    assert!(snap_common(Some(Path::new("/usr/bin/snap"))).is_some());
    assert!(snap_common(Some(Path::new("/usr/lib/snapd/snapd"))).is_some());
    assert!(snap_common(Some(Path::new("/snap/ecrou/current/bin/ecrou"))).is_some());
    assert!(snap_common(Some(Path::new("/usr/local/bin/ecrou"))).is_none());
    assert!(snap_common(Some(Path::new("/home/u/.cargo/bin/ecrou"))).is_none());
    assert!(snap_common(Some(Path::new("/snapd/ecrou"))).is_none());
    assert!(
        snap_common(None).is_none(),
        "an unresolved binary stays native"
    );
}

fn sandbox() -> (EcrouSandbox, tempfile::TempDir) {
    let dir = scratch_dir().expect("tempdir");
    let root = FsRoot::new(dir.path().to_path_buf()).expect("the sandbox root builds");
    let ws = dir.path().to_string_lossy().into_owned();
    (EcrouSandbox::new("alpine:3.23", root, ws), dir)
}

fn request(command: &str) -> RunRequest {
    RunRequest {
        command: command.to_owned(),
        cwd: "/".into(),
        timeout: Duration::from_secs(30),

        ..Default::default()
    }
}

async fn run(sandbox: &EcrouSandbox, request: RunRequest) -> Result<RunOutcome, SandboxError> {
    sandbox
        .run(request, &AtomicBool::new(false), &mut OutputOnly(|_| {}))
        .await
}

#[test]
fn the_payload_line_prefers_bash_and_falls_back_to_sh() {
    let line = shell_line("echo hi");
    assert!(line.starts_with("if command -v bash >/dev/null 2>&1; then"));
    assert!(line.contains("exec bash -c 'echo hi'"));
    assert!(line.ends_with("else exec sh -c 'echo hi'; fi"));
}

#[test]
fn the_persist_state_rides_as_run_flags_ahead_of_the_image() {
    let argv = run_argv(
        "ecrou",
        "ubuntu",
        Path::new("/tmp/io"),
        &["/w:/w"],
        "/w",
        "echo hi",
        &BTreeMap::new(),
        None,
        Some("proj-1"),
        false,
    );
    let at = argv
        .iter()
        .position(|a| a == "--persist")
        .expect("the persist pair is in the argv");
    assert_eq!(argv[at + 1], "proj-1");
    assert!(at < argv.iter().position(|a| a == "ubuntu").unwrap());

    let reset = run_argv(
        "ecrou",
        "ubuntu",
        Path::new("/tmp/io"),
        &["/w:/w"],
        "/w",
        "echo hi",
        &BTreeMap::new(),
        None,
        Some("proj-1"),
        true,
    );
    let at = reset
        .iter()
        .position(|a| a == "--persist-reset")
        .expect("the reset flag is in the argv");
    assert_eq!(reset[at + 1], "proj-1");
    assert!(!reset.contains(&"--persist".to_owned()));

    let none = run_argv(
        "ecrou",
        "ubuntu",
        Path::new("/tmp/io"),
        &["/w:/w"],
        "/w",
        "echo hi",
        &BTreeMap::new(),
        None,
        None,
        false,
    );
    assert!(!none.contains(&"--persist".to_owned()));
    assert!(!none.contains(&"--persist-reset".to_owned()));
}

#[test]
fn the_command_is_quoted_inside_the_payload_line() {
    let line = shell_line("echo it's; rm -rf $HOME");
    assert!(
        line.contains(r"'echo it'\''s; rm -rf $HOME'"),
        "the command rides as one quoted operand: {line}"
    );
}

#[test]
fn the_run_argv_names_capture_share_workdir_then_the_payload() {
    let argv = run_argv(
        "ecrou",
        "ubuntu",
        Path::new("/tmp/io"),
        &["/home/me/proj:/home/me/proj"],
        "/home/me/proj/src",
        "echo hi",
        &BTreeMap::new(),
        None,
        None,
        false,
    );
    let expected = [
        "ecrou",
        "run",
        "--quiet",
        "--capture",
        "/tmp/io",
        "-v",
        "/home/me/proj:/home/me/proj",
        "--workdir",
        "/home/me/proj/src",
        "ubuntu",
        "--",
        "/bin/sh",
        "-c",
    ] as [&str; 13];
    assert_eq!(&argv[..expected.len()], expected);
    assert_eq!(argv.len(), expected.len() + 1);
    assert!(argv[argv.len() - 1].starts_with("if command -v bash"));
}

#[test]
fn the_request_env_rides_as_pairs_ahead_of_the_image() {
    let env = BTreeMap::from([(
        "RHEMA_SEARCH_CHAIN".to_owned(),
        "[{\"name\":\"exa\",\"provider\":\"exa\",\"key\":\"k\"}]".to_owned(),
    )]);
    let argv = run_argv(
        "ecrou",
        "ubuntu",
        Path::new("/tmp/io"),
        &["/w:/w"],
        "/w",
        "echo hi",
        &env,
        None,
        None,
        false,
    );
    let at = argv
        .iter()
        .position(|a| a == "--env")
        .expect("the env pair is in the argv");
    assert_eq!(
        argv[at + 1],
        "RHEMA_SEARCH_CHAIN=[{\"name\":\"exa\",\"provider\":\"exa\",\"key\":\"k\"}]"
    );
    assert!(at < argv.iter().position(|a| a == "ubuntu").unwrap());
}

#[test]
fn the_runtime_share_rides_beside_the_workspace() {
    let argv = run_argv(
        "ecrou",
        "ubuntu",
        Path::new("/tmp/io"),
        &[
            "/home/me/proj:/home/me/proj",
            "/run/user/1000/rhema:/run/user/1000/rhema",
        ],
        "/home/me/proj",
        "echo hi",
        &BTreeMap::new(),
        None,
        None,
        false,
    );
    let shares: Vec<&str> = argv
        .windows(2)
        .filter(|pair| pair[0] == "-v")
        .map(|pair| pair[1].as_str())
        .collect();
    assert_eq!(
        shares,
        [
            "/home/me/proj:/home/me/proj",
            "/run/user/1000/rhema:/run/user/1000/rhema"
        ]
    );
}

/// The staged env rides as one `--env GIT_CONFIG_GLOBAL=...` pair
/// ahead of the image, and without a staged config there is no
/// pair: git inside behaves as it always has.
#[test]
fn the_run_argv_points_git_at_the_staged_config_only_when_staged() {
    let env = GitEnv {
        guest_config_path: "/home/me/proj/.rhema/git/config".to_owned(),
    };
    let argv = run_argv(
        "ecrou",
        "ubuntu",
        Path::new("/tmp/io"),
        &["/home/me/proj:/home/me/proj"],
        "/home/me/proj",
        "echo hi",
        &BTreeMap::new(),
        Some(&env),
        None,
        false,
    );
    let at = argv
        .iter()
        .position(|a| a == "--env")
        .expect("the env pair is in the argv");
    assert_eq!(
        argv[at + 1],
        "GIT_CONFIG_GLOBAL=/home/me/proj/.rhema/git/config"
    );
    // ahead of the image, so it is run's flag, not the payload's
    assert!(at < argv.iter().position(|a| a == "ubuntu").unwrap());

    let bare = run_argv(
        "ecrou",
        "ubuntu",
        Path::new("/tmp/io"),
        &["/w:/w"],
        "/w",
        "echo hi",
        &BTreeMap::new(),
        None,
        None,
        false,
    );
    assert!(!bare.contains(&"--env".to_owned()));
}

/// The staging: a host gitconfig flattens into the workspace under
/// the guard overrides, the exclude keeps it out of status, and a
/// second staging writes nothing new.
#[test]
fn the_host_git_config_stages_flattened_and_guarded() {
    let home = scratch_dir().expect("tempdir");
    let (_sandbox, dir) = sandbox();
    fixture_repo_at(dir.path());
    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]
	name = Host User
	email = host@example.com
[alias]
	st = status
[commit]
	gpgsign = true
[credential]
	helper = /usr/bin/host-helper
",
    )
    .expect("the host config is written");

    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["config", "--list", "-z", "--includes"])
        .env("HOME", home.path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(output.status.success());
    let staged = render_pairs(&String::from_utf8_lossy(&output.stdout));
    assert!(staged.contains("[user]\n\tname = Host User\n"));
    assert!(staged.contains("email = host@example.com"));
    assert!(staged.contains("[alias]\n\tst = status"));

    let full = format!("{staged}{}", GIT_GUARDS);
    let out = dir.path().join(".rhema/git/config");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(&out, &full).unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    // the guards always win, whichever way the host config leaned
    assert!(text.contains("[safe]\n\tdirectory = *"));
    assert!(text.contains("gpgsign = false"));
    assert!(text.contains("helper =\n"));
    // and git re-reads the whole thing without complaint
    let check = std::process::Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["config", "--list"])
        .env("GIT_CONFIG_GLOBAL", &out)
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("HOME", home.path())
        .output()
        .expect("git runs");
    assert!(
        check.status.success(),
        "git rejects the staged file: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    let reflat = String::from_utf8_lossy(&check.stdout);
    assert!(reflat.contains("user.name=Host User"));
    assert!(reflat.contains("commit.gpgsign=false"));
}

/// Quoted values come back the way they went in: a config whose
/// alias carries a newline round-trips through the staged file.
#[test]
fn multiline_values_survive_the_round_trip() {
    let staged = render_pairs("alias.multi\nline one\nline two\0user.name\nU\0");
    assert!(
        staged.contains("[alias]\n\tmulti = \"line one\\nline two\""),
        "{staged}"
    );
    assert!(staged.contains("[user]\n\tname = U"), "{staged}");
}

/// The whole git chain: a commit made inside the machine lands in
/// the host's log, authored as the host's user, the staged config
/// crossing the share, the env pair pointing git at it, the
/// dubious-ownership guard letting root touch a foreign tree.
/// Skipped until the machine's image carries git: a pristine
/// distro tarball has none, and the provisioning that adds it is
/// the assembly's business, not this test's download.
#[tokio::test]
async fn a_persisted_state_carries_a_write_across_two_boots() {
    if !ecrou_ready() {
        println!("skipping: no runnable ecrou or no cached alpine image");
        return;
    }
    let (dir, key) = {
        let dir = scratch_dir().expect("tempdir");
        let key = format!(
            "rhema-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        (dir, key)
    };
    // The state is the test's own; a killed run must not leave it
    // behind for the next run of the suite.
    // The state files carry an arch in their name; the prefix is
    // the stable part, so cleanup walks the cache directory.
    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let prefix = format!("persist-{}-", self.0);
            if let Some(home) = std::env::var_os("HOME") {
                let cache = std::path::PathBuf::from(home).join(".cache/ecrou");
                if let Ok(entries) = std::fs::read_dir(&cache) {
                    for entry in entries.flatten() {
                        if entry.file_name().to_string_lossy().starts_with(&prefix) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
    }
    let _cleanup = Cleanup(key.clone());

    let root = FsRoot::new(dir.path().to_path_buf()).expect("the sandbox root builds");
    let ws = dir.path().to_string_lossy().into_owned();
    let sandbox = EcrouSandbox::new("alpine:3.23", root, ws).with_persist(key);

    // A rootfs write outside the workspace share, committed by a
    // clean exit: the next boot starts from it.
    let first = run(
        &sandbox,
        RunRequest {
            command: "mkdir -p /opt && echo persisted > /opt/marker && echo wrote".into(),
            cwd: "/".into(),
            timeout: Duration::from_secs(60),

            ..Default::default()
        },
    )
    .await
    .expect("the first boot");
    assert_eq!(first.code, Some(0), "first boot: {:?}", first.stderr);

    let second = run(
        &sandbox,
        RunRequest {
            command: "cat /opt/marker".into(),
            cwd: "/".into(),
            timeout: Duration::from_secs(60),

            ..Default::default()
        },
    )
    .await
    .expect("the second boot");
    assert_eq!(second.code, Some(0), "second boot: {:?}", second.stderr);
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("persisted"),
        "the write crossed the boots: {:?}",
        second.stdout
    );

    // A failed command drops its changes: the state stays what the
    // last clean run left.
    let dirty = run(
        &sandbox,
        RunRequest {
            command: "echo dirty > /opt/marker; exit 3".into(),
            cwd: "/".into(),
            timeout: Duration::from_secs(60),

            ..Default::default()
        },
    )
    .await
    .expect("the dirty boot");
    assert_eq!(dirty.code, Some(3));

    let after = run(
        &sandbox,
        RunRequest {
            command: "cat /opt/marker".into(),
            cwd: "/".into(),
            timeout: Duration::from_secs(60),

            ..Default::default()
        },
    )
    .await
    .expect("the boot after the dirty one");
    assert!(
        !String::from_utf8_lossy(&after.stdout).contains("dirty"),
        "a failed command must not commit: {:?}",
        after.stdout
    );
}

#[tokio::test]
async fn a_commit_inside_the_machine_is_the_hosts_commit() {
    if !ecrou_ready() {
        println!("skipping: no runnable ecrou or no cached alpine image");
        return;
    }
    let (sandbox, dir) = sandbox();
    fixture_repo_at(dir.path());
    let git_present = run(
        &sandbox,
        RunRequest {
            command: "command -v git".into(),
            cwd: dir.path().to_owned(),
            timeout: Duration::from_secs(30),

            ..Default::default()
        },
    )
    .await
    .map(|outcome| outcome.code == Some(0))
    .unwrap_or(false);
    if !git_present {
        println!("skipping: the machine's image does not carry git yet");
        return;
    }

    // The host identity comes from a scratch HOME, so the test
    // proves the staging, not the developer's own config.
    let home = scratch_dir().expect("tempdir");
    std::fs::write(
        home.path().join(".gitconfig"),
        b"[user]\n\tname = Machine Author\n\temail = machine@example.com\n",
    )
    .expect("the scratch gitconfig is written");
    let root = dir.path();
    let flat = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--list", "-z", "--includes"])
        .env("HOME", home.path())
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(flat.status.success());
    let mut staged = render_pairs(&String::from_utf8_lossy(&flat.stdout));
    staged.push_str(GIT_GUARDS);
    let staged_dir = root.join(".rhema/git");
    std::fs::create_dir_all(&staged_dir).expect("the staging directory is created");
    std::fs::write(staged_dir.join("config"), &staged).expect("the staged config is written");

    let outcome = run(
        &sandbox,
        RunRequest {
            command: "git commit -q --allow-empty -m from-the-machine".into(),
            cwd: root.to_owned(),
            timeout: Duration::from_secs(60),

            ..Default::default()
        },
    )
    .await
    .expect("the machine runs");
    assert_eq!(
        outcome.code,
        Some(0),
        "the commit inside the machine: {outcome:?}"
    );

    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["log", "--format=%an <%ae> %s", "-1"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    let line = String::from_utf8_lossy(&log.stdout);
    assert!(
        line.contains("Machine Author <machine@example.com> from-the-machine"),
        "the host log names the machine's commit: {line}"
    );
}

#[test]
fn append_capped_keeps_only_the_last_cap() {
    let mut tail = Vec::new();
    append_capped(&mut tail, &[b'x'; EXEC_TAIL_BYTES * 3]);
    assert_eq!(tail.len(), EXEC_TAIL_BYTES);
}

#[tokio::test]
async fn the_tail_pump_reads_complete_lines_and_holds_the_partial() {
    let dir = scratch_dir().expect("tempdir");
    let path = dir.path().join("out");
    std::fs::write(&path, b"first\nsec").expect("the first chunk is written");
    let mut stream = StreamTail {
        path,
        ..StreamTail::default()
    };
    let mut lines: Vec<String> = Vec::new();
    stream
        .pump(&mut OutputOnly(|line| lines.push(line.to_owned())), false)
        .await
        .expect("the first pump reads");
    assert_eq!(lines, vec!["first\n".to_owned()]);
    // The partial line is still on disk, unread; the next chunk
    // completes it.
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&stream.path)
        .expect("the file reopens");
    file.write_all(b"ond\n").expect("the second chunk lands");
    drop(file);
    stream
        .pump(&mut OutputOnly(|line| lines.push(line.to_owned())), false)
        .await
        .expect("the second pump reads");
    assert_eq!(lines, vec!["first\n".to_owned(), "second\n".to_owned()]);
    assert_eq!(stream.take(), b"first\nsecond\n".to_vec());
}

#[tokio::test]
async fn the_final_pass_takes_a_trailing_partial_line() {
    let dir = scratch_dir().expect("tempdir");
    let path = dir.path().join("out");
    std::fs::write(&path, b"no newline at the end").expect("the chunk is written");
    let mut stream = StreamTail {
        path,
        ..StreamTail::default()
    };
    let mut lines: Vec<String> = Vec::new();
    stream
        .pump(&mut OutputOnly(|line| lines.push(line.to_owned())), true)
        .await
        .expect("the final pump reads");
    assert_eq!(lines, vec!["no newline at the end\n".to_owned()]);
    assert_eq!(stream.take(), b"no newline at the end\n".to_vec());
}

#[tokio::test]
async fn the_tail_pump_treats_an_absent_file_as_silence() {
    let dir = scratch_dir().expect("tempdir");
    let mut stream = StreamTail {
        path: dir.path().join("never-written"),
        ..StreamTail::default()
    };
    let mut lines: Vec<String> = Vec::new();
    stream
        .pump(&mut OutputOnly(|line| lines.push(line.to_owned())), false)
        .await
        .expect("absence is silence");
    assert!(lines.is_empty());
}

#[test]
fn an_ecrou_error_reads_as_an_unavailable_sandbox() {
    let error = classify(
        Some(1),
        "Warning: could not check image freshness\nError: the machine failed to boot\n".to_owned(),
        Vec::new(),
        Vec::new(),
    )
    .expect_err("the boot failure is an unavailable sandbox");
    assert_eq!(
        error,
        SandboxError::Unavailable("the machine failed to boot".to_owned())
    );
}

#[test]
fn a_stale_binary_reads_as_an_unavailable_sandbox() {
    let error = classify(
        Some(2),
        "error: unexpected argument '--capture' found\n\nUsage: ecrou run --quiet <IMAGE>\n"
            .to_owned(),
        Vec::new(),
        Vec::new(),
    )
    .expect_err("the usage error is an unavailable sandbox");
    assert_eq!(
        error,
        SandboxError::Unavailable("unexpected argument '--capture' found".to_owned())
    );
}

#[test]
fn a_guest_exit_rides_with_warnings_ignored() {
    let outcome = classify(
        Some(3),
        "Warning: could not check image freshness (dns); using cache\n".to_owned(),
        b"out".to_vec(),
        b"err".to_vec(),
    )
    .expect("the guest exit is the command's own result");
    assert_eq!(outcome.code, Some(3));
    assert_eq!(outcome.stdout, b"out");
    assert_eq!(outcome.stderr, b"err");
}

#[test]
fn a_zero_exit_rides_whatever_stderr_says() {
    let outcome = classify(Some(0), String::new(), b"ok\n".to_vec(), Vec::new())
        .expect("a zero exit is a done command");
    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stdout, b"ok\n");
}

#[tokio::test]
async fn a_flip_before_the_start_never_boots_a_machine() {
    let (sandbox, _dir) = sandbox();
    let cancelled = AtomicBool::new(true);
    let error = sandbox
        .run(request("echo hi"), &cancelled, &mut OutputOnly(|_| {}))
        .await
        .expect_err("a cancelled run does not boot");
    assert_eq!(error, SandboxError::Failed("cancelled".into()));
}

#[tokio::test]
async fn a_dot_dot_working_directory_is_refused() {
    let (sandbox, _dir) = sandbox();
    let error = run(
        &sandbox,
        RunRequest {
            command: "pwd".into(),
            cwd: "/../etc".into(),
            timeout: Duration::from_secs(30),

            ..Default::default()
        },
    )
    .await
    .expect_err("the climb is refused");
    assert!(
        matches!(error, SandboxError::Failed(_)),
        "the refusal is an in-band failure, not a crash: {error:?}"
    );
}

#[test]
fn the_guest_cwd_joins_the_mount_point() {
    let dir = scratch_dir().expect("tempdir");
    let root = FsRoot::new(dir.path().to_path_buf()).expect("the root builds");
    let mount = std::fs::canonicalize(dir.path())
        .expect("the tempdir resolves")
        .to_string_lossy()
        .into_owned();
    let sandbox = EcrouSandbox::new("alpine:3.23", root, mount.clone());
    assert_eq!(sandbox.guest_cwd("").expect("the empty cwd maps"), mount);
    assert_eq!(sandbox.guest_cwd("/").expect("the root maps"), mount);
    assert_eq!(
        sandbox
            .guest_cwd(&format!("{mount}/src/deep"))
            .expect("the nested cwd maps"),
        format!("{mount}/src/deep")
    );
}

// The live-machine tests below boot one alpine each; they skip
// where ecrou cannot run or the image is not cached, and the
// readiness check keeps the suite honest about which one it was.

#[tokio::test]
async fn a_command_output_and_its_exit_code_cross_the_capture() {
    if !ecrou_ready() {
        println!("skipping: no runnable ecrou or no cached alpine image");
        return;
    }
    let (sandbox, _dir) = sandbox();
    let mut lines: Vec<String> = Vec::new();
    let outcome = {
        let cancelled = AtomicBool::new(false);
        sandbox
            .run(
                request("echo from the vm; echo to err 1>&2; exit 7"),
                &cancelled,
                &mut OutputOnly(|line| lines.push(line.to_owned())),
            )
            .await
            .expect("the machine runs")
    };
    assert_eq!(outcome.code, Some(7));
    assert_eq!(outcome.stdout, b"from the vm\n");
    assert_eq!(outcome.stderr, b"to err\n");
    assert!(
        lines.contains(&"from the vm\n".to_owned()),
        "the line streamed through progress: {lines:?}"
    );
}

#[tokio::test]
async fn the_shared_workspace_is_the_same_tree_both_sides() {
    if !ecrou_ready() {
        println!("skipping: no runnable ecrou or no cached alpine image");
        return;
    }
    let (sandbox, dir) = sandbox();
    // The host write and the guest read meet through the share:
    // the sandbox path /note.txt is <root>/note.txt host-side and
    // <mount>/note.txt guest-side, reached over the workspace cwd.
    sandbox
        .write_file("/note.txt", b"from the host\n")
        .await
        .expect("the write lands");
    let ws = dir.path().to_string_lossy().into_owned();
    let outcome = run(
        &sandbox,
        RunRequest {
            command: "cat note.txt && echo guest >> note.txt".into(),
            cwd: ws.into(),
            timeout: Duration::from_secs(30),

            ..Default::default()
        },
    )
    .await
    .expect("the machine runs");
    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stdout, b"from the host\n");
    assert_eq!(
        std::fs::read(dir.path().join("note.txt")).expect("the host side reads"),
        b"from the host\nguest\n"
    );
}

#[tokio::test]
async fn a_command_past_its_budget_is_a_timeout_and_the_machine_dies() {
    if !ecrou_ready() {
        println!("skipping: no runnable ecrou or no cached alpine image");
        return;
    }
    let (sandbox, _dir) = sandbox();
    let started = Instant::now();
    let error = run(
        &sandbox,
        RunRequest {
            command: "sleep 60".into(),
            cwd: "/".into(),
            timeout: Duration::from_secs(2),

            ..Default::default()
        },
    )
    .await
    .expect_err("the budget kills the command");
    assert_eq!(error, SandboxError::Timeout(2));
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the kill lands near the budget, the boot inside it: {:?}",
        started.elapsed()
    );
}

/// A fake `ecrou` script: the first boot (`--persist`) burns most
/// of the budget with a real sleep, then refuses on the mismatch;
/// the reset boot (`--persist-reset`) tries to run its own fresh
/// budget of sleep. If the retry shares the deadline, the second
/// boot dies inside the first budget's remainder; a fresh clock
/// would let it finish.
fn fake_machine(dir: &Path, first_boot: f64, second_boot: f64) -> std::path::PathBuf {
    let body = format!(
        "#!/bin/sh\n             case \"$*\" in\n               *--persist-reset*) sleep {second_boot}; exit 0;;\n\
               *) sleep {first_boot}; echo 'Error: persist base mismatch' >&2; exit 3;;\n\
             esac\n"
    );
    let path = dir.join("ecrou-fake");
    std::fs::write(&path, body).expect("the fake is written");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("the fake is executable");
    path
}

#[tokio::test]
async fn the_persist_retry_does_not_extend_the_budget() {
    let (mut sandbox, dir) = sandbox();
    sandbox.bin = fake_machine(dir.path(), 1.4, 1.4).display().to_string();
    sandbox.persist = Some("proj-1".to_owned());
    // The budget that pays for both boots together: the first
    // boot spends 1.4s of it, and the retry must die inside the
    // remainder instead of sleeping its own fresh 1.4s out.
    let started = Instant::now();
    let error = run(
        &sandbox,
        RunRequest {
            command: "true".into(),
            cwd: "/".into(),
            timeout: Duration::from_secs(2),

            ..Default::default()
        },
    )
    .await
    .expect_err("one budget cannot pay for two boots");
    assert_eq!(error, SandboxError::Timeout(2));
    assert!(
        started.elapsed() < Duration::from_millis(2800),
        "the retry restarted the wall clock: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_flip_mid_run_kills_the_machine() {
    if !ecrou_ready() {
        println!("skipping: no runnable ecrou or no cached alpine image");
        return;
    }
    let (sandbox, _dir) = sandbox();
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        flag.store(true, Ordering::Relaxed);
    });
    let started = Instant::now();
    let error = sandbox
        .run(request("sleep 60"), &cancelled, &mut OutputOnly(|_| {}))
        .await
        .expect_err("the flip stops the run");
    assert_eq!(error, SandboxError::Failed("cancelled".into()));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the machine dies on the flip, not at its own pace: {:?}",
        started.elapsed()
    );
}

/// GNU `timeout` is a Linux tool: a machine without it in PATH
/// cannot exercise the seed route's prefix, and these tests skip.
fn have_timeout() -> bool {
    std::process::Command::new("timeout")
        .arg("--version")
        .output()
        .is_ok()
}

#[tokio::test]
async fn seeding_from_a_local_repository_populates_the_root() {
    if !have_timeout() {
        eprintln!("skipping: no GNU timeout on this machine");
        return;
    }
    let repo = fixture_repo();
    let (sandbox, dir) = sandbox();
    sandbox
        .seed(&WorkspaceSeed::GitClone(
            repo.path().to_string_lossy().into_owned(),
        ))
        .await
        .expect("the clone lands");
    assert!(
        dir.path().join(".git").exists(),
        "the clone is rooted at the sandbox root"
    );
    assert_eq!(
        std::fs::read(dir.path().join("seeded.txt")).expect("the seeded file is read"),
        b"from git"
    );
}

#[tokio::test]
async fn a_failed_clone_fails_in_band() {
    if !have_timeout() {
        eprintln!("skipping: no GNU timeout on this machine");
        return;
    }
    let (sandbox, _dir) = sandbox();
    let error = sandbox
        .seed(&WorkspaceSeed::GitClone("/nonexistent-rhema-repo".into()))
        .await
        .expect_err("the clone fails");
    assert!(
        matches!(error, SandboxError::Failed(_)),
        "a failed clone is an in-band failure: {error:?}"
    );
}

#[tokio::test]
async fn seeding_from_a_tarball_unpacks_into_the_root() {
    if !have_timeout() {
        eprintln!("skipping: no GNU timeout on this machine");
        return;
    }
    let payload = scratch_dir().expect("tempdir");
    std::fs::write(payload.path().join("packed.txt"), b"from tar")
        .expect("the payload file is written");
    let archive = payload.path().join("packed.tar.gz");
    let status = std::process::Command::new("tar")
        .arg("czf")
        .arg(&archive)
        .arg("-C")
        .arg(payload.path())
        .arg("packed.txt")
        .status()
        .expect("tar runs");
    assert!(status.success(), "the fixture tarball is built");
    let bytes = std::fs::read(&archive).expect("the tarball is read");
    let (sandbox, dir) = sandbox();
    sandbox
        .seed(&WorkspaceSeed::Tarball {
            name: "packed.tar.gz".into(),
            tar_gz_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
        .await
        .expect("the unpack lands");
    assert_eq!(
        std::fs::read(dir.path().join("packed.txt")).expect("the unpacked file is read"),
        b"from tar"
    );
    assert!(
        !dir.path().join(STAGING).exists(),
        "the staging file does not linger"
    );
}

/// Builds a one-commit repository at a fresh tempdir, the fixture
/// the git-seed test clones. Skips the test when git or the
/// GNU `timeout` the seed budget leans on are missing.
fn fixture_repo() -> tempfile::TempDir {
    let dir = scratch_dir().expect("tempdir");
    fixture_repo_at(dir.path());
    dir
}

/// The same repository, in a directory the caller owns.
fn fixture_repo_at(dir: &Path) {
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("seeded.txt"), b"from git").expect("the fixture file is written");
    git(&["add", "."]);
    git(&[
        "-c",
        "user.email=test@example.com",
        "-c",
        "user.name=test",
        "commit",
        "-q",
        "-m",
        "seed",
    ]);
}
