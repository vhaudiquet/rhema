use super::*;

#[test]
fn a_shell_line_wrapped_for_the_backend_comes_back_as_written() {
    let wrapped = vec!["bash".to_owned(), "-c".to_owned(), "npm run dev".to_owned()];
    assert_eq!(unwrap_command(&wrapped), "npm run dev");
}

#[test]
fn a_command_started_outside_the_shim_shows_its_argv() {
    let bare = vec!["npm".to_owned(), "run".to_owned(), "dev".to_owned()];
    assert_eq!(unwrap_command(&bare), "npm run dev");
}

#[test]
fn the_session_is_read_from_the_tag() {
    assert_eq!(
        owner_of(Some("[s7f2] npm run dev")),
        Some("s7f2".to_owned())
    );
    assert_eq!(owner_of(Some("a note with no tag")), None);
    assert_eq!(owner_of(Some("[unclosed")), None);
    assert_eq!(owner_of(Some("[] empty")), None);
    assert_eq!(owner_of(None), None);
}

#[test]
fn a_tag_longer_than_an_id_is_not_a_session() {
    let long = format!("[{}] cmd", "s".repeat(MAX_ID_CHARS + 1));
    assert_eq!(owner_of(Some(&long)), None);
}

#[test]
fn a_started_job_announces_its_id() {
    assert_eq!(
        parse_added_id("Added job M8c running: bash -c npm run dev\n"),
        Some("M8c".to_owned())
    );
    assert_eq!(
        parse_added_id("Job 8Pc already running (since 877ms ago)\n"),
        Some("8Pc".to_owned())
    );
    assert_eq!(parse_added_id("something else\n"), None);
}

#[test]
fn an_event_line_parses_into_its_row() {
    let added = r#"{"type":"job_added","job_id":"K62","job":{"id":"K62","pid":1916773,"status":"running","command":["bash","-c","echo hi; sleep 1"],"workdir":"/tmp","description":"[probe] sleep demo","created_at":"2026-11-21T18:58:31+01:00","started_at":"2026-11-21T18:58:31+01:00"}}"#;
    let event = parse_event_line(added).expect("an added line parses");
    assert_eq!(event.kind, EventKind::Added);
    assert_eq!(event.row.id, "K62");
    assert_eq!(event.row.command, "echo hi; sleep 1");
    assert_eq!(event.row.session.as_deref(), Some("probe"));
    assert_eq!(event.row.exit_code, None, "nothing has ended yet");
}

#[test]
fn a_stop_event_carries_the_exit_code() {
    let stopped = r#"{"type":"job_stopped","job_id":"K62","job":{"id":"K62","pid":1916773,"status":"stopped","command":["bash","-c","echo hi"],"workdir":"/tmp","description":"[probe] sleep demo","exit_code":0,"started_at":"2026-11-21T18:58:31+01:00","stopped_at":"2026-11-21T18:58:32+01:00"}}"#;
    let event = parse_event_line(stopped).expect("a stopped line parses");
    assert_eq!(event.kind, EventKind::Stopped);
    assert_eq!(event.row.exit_code, Some(0));
}

#[test]
fn the_run_level_twins_and_unknown_lines_are_ignored() {
    let run = r#"{"type":"run_started","job_id":"K62","run":{"id":"K62-1","status":"running"}}"#;
    assert_eq!(parse_event_line(run), None, "the job-level line follows");
    assert_eq!(parse_event_line("not json at all"), None);
    assert_eq!(
        parse_event_line(r#"{"type":"daemon_started"}"#),
        None,
        "an unknown type is skipped, not a fault"
    );
    assert_eq!(
        parse_event_line(r#"{"type":"job_added"}"#),
        None,
        "an event with no job payload carries nothing this module reads"
    );
}

#[test]
fn every_job_level_event_kind_parses() {
    for (kind, text) in [
        (EventKind::Added, "job_added"),
        (EventKind::Started, "job_started"),
        (EventKind::Stopped, "job_stopped"),
        (EventKind::Removed, "job_removed"),
    ] {
        let line = format!(
            r#"{{"type":"{text}","job":{{"id":"abc","status":"running","command":["bash","-c","x"]}}}}"#
        );
        let event = parse_event_line(&line).expect("the kind parses");
        assert_eq!(event.kind, kind);
        assert_eq!(event.row.id, "abc");
    }
}

/// A workspace with a fake backend: a shell script that logs every argv it
/// gets and answers `list` from a state file. The paths are baked into the
/// script, so no test mutates process-wide environment or cwd.
#[cfg(unix)]
struct Fake {
    dir: tempfile::TempDir,
    backend: PathBuf,
    log: PathBuf,
    state: PathBuf,
}

/// A backend script, settled: a parallel test can fork while this file's
/// write descriptor is still open, and the child's inherited copy keeps
/// the inode unexecutable until it execs (`ETXTBSY`). One successful
/// exec here proves the window has closed for every later one.
#[cfg(unix)]
fn script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, body).expect("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    for _ in 0..100 {
        if std::process::Command::new(&path)
            .arg("--version")
            .output()
            .is_ok()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    path
}

#[cfg(unix)]
impl Fake {
    fn new(listing: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("calls.log");
        let state = dir.path().join("state.json");
        std::fs::write(&state, listing).expect("state");
        let backend = script(
            dir.path(),
            "gob",
            &format!(
                "#!/bin/sh\n\
                 case \"$1\" in\n\
                   --version) echo 'gob version 0.0.0'; exit 0;;\n\
                 esac\n\
                 echo \"$@\" >> '{}'\n\
                 case \"$1\" in\n\
                   list) cat '{}'; exit 0;;\n\
                   add) echo 'Added job new running: bash -c x'; exit 0;;\n\
                   stop) echo 'Stopped '$2; exit 0;;\n\
                   *) exit 0;;\n\
                 esac\n",
                log.display(),
                state.display()
            ),
        );
        Self {
            dir,
            backend,
            log,
            state,
        }
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn set_listing(&self, listing: &str) {
        std::fs::write(&self.state, listing).expect("state");
    }
}

/// `run_cli`, retried while the freshly written script is still
/// `ETXTBSY`: a test running in parallel can fork while this one holds an
/// open write descriptor on the script, and the child's inherited copy
/// keeps the inode unexecutable until it execs. The window is a few
/// milliseconds; the retry closes it without serializing the suite.
#[cfg(unix)]
fn call(fake: &Fake, argv: &[OsString], session: Option<&str>) -> i32 {
    let mut code = EXIT_NO_BACKEND;
    for _ in 0..50 {
        code = run_cli(argv, &fake.backend, fake.dir.path(), session);
        if code != EXIT_NO_BACKEND {
            return code;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    code
}

/// [`owned_rows`], retried for the same reason [`call`] retries.
#[cfg(unix)]
fn rows(fake: &Fake, scope: Scope, session: Option<&str>) -> Vec<JobRow> {
    for _ in 0..50 {
        if let Ok(rows) = owned_rows(&fake.backend, fake.dir.path(), scope, session) {
            return rows;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    owned_rows(&fake.backend, fake.dir.path(), scope, session).expect("rows")
}

#[cfg(unix)]
fn listing(entries: &str) -> String {
    format!("[{entries}]")
}

#[cfg(unix)]
fn entry(id: &str, status: &str, description: &str, exit_code: Option<i32>) -> String {
    entry_in(id, status, description, exit_code, None)
}

#[cfg(unix)]
fn entry_with_command(id: &str, status: &str, description: &str, command: &str) -> String {
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "{{\"id\":\"{id}\",\"status\":\"{status}\",\"command\":[\"bash\",\"-c\",\"{escaped}\"],\
         \"description\":\"{description}\",\"exit_code\":null,\"workdir\":null}}"
    )
}

#[cfg(unix)]
fn entry_in(
    id: &str,
    status: &str,
    description: &str,
    exit_code: Option<i32>,
    workdir: Option<&str>,
) -> String {
    let exit = exit_code.map_or_else(|| "null".to_owned(), |code| code.to_string());
    let workdir = workdir.map_or_else(|| "null".to_owned(), |dir| format!("\"{dir}\""));
    format!(
        "{{\"id\":\"{id}\",\"status\":\"{status}\",\"command\":[\"bash\",\"-c\",\"npm run dev\"],\
         \"description\":\"{description}\",\"exit_code\":{exit},\
         \"workdir\":{workdir},\
         \"started_at\":\"2026-11-11T11:20:38+01:00\"}}"
    )
}

#[cfg(unix)]
#[test]
fn a_second_session_cannot_take_over_a_running_line() {
    let fake = Fake::new(&listing(&entry("abc", "running", "[s1] npm run dev", None)));
    let argv = vec![
        OsString::from("job"),
        OsString::from("run"),
        OsString::from("npm"),
        OsString::from("run"),
        OsString::from("dev"),
    ];
    let code = call(&fake, &argv, Some("s2"));
    assert_eq!(code, 0);
    // Refused: the backend never saw an `add`, so its dedupe cannot
    // rewrite the owner's description.
    assert!(
        !fake.calls().contains("add"),
        "the shim added a second job: {}",
        fake.calls()
    );
}

#[cfg(unix)]
#[test]
fn the_same_session_asking_again_is_told_the_id() {
    let fake = Fake::new(&listing(&entry("abc", "running", "[s1] npm run dev", None)));
    let argv = vec![
        OsString::from("job"),
        OsString::from("run"),
        OsString::from("npm"),
        OsString::from("run"),
        OsString::from("dev"),
    ];
    let code = call(&fake, &argv, Some("s1"));
    assert_eq!(code, 0);
    assert!(!fake.calls().contains("add"), "{}", fake.calls());
}

#[cfg(unix)]
#[test]
fn a_free_line_starts_through_the_shell_wrapper() {
    let fake = Fake::new(&listing(""));
    let argv = vec![
        OsString::from("job"),
        OsString::from("run"),
        OsString::from("npm"),
        OsString::from("run"),
        OsString::from("dev"),
    ];
    let code = call(&fake, &argv, Some("s7"));
    assert_eq!(code, 0);
    let calls = fake.calls();
    assert!(
        calls.contains("add --description [s7] npm run dev -- bash -c npm run dev"),
        "the job did not start wrapped and tagged: {calls}"
    );
}

#[test]
fn a_shell_word_that_needs_no_quotes_stays_bare() {
    // The plain word list reads back exactly as the model wrote it,
    // which is what the dedupe and the `ls` column compare against.
    assert_eq!(
        ["npm", "run", "dev"]
            .iter()
            .map(|w| requote(w))
            .collect::<Vec<_>>()
            .join(" "),
        "npm run dev"
    );
}

#[test]
fn a_word_with_spaces_is_quoted_back_together() {
    // `job run echo "a  b"` hands the shim two words, and the line
    // the backend runs must keep the double space as one argument.
    assert_eq!(
        ["echo", "a  b"]
            .iter()
            .map(|w| requote(w))
            .collect::<Vec<_>>()
            .join(" "),
        "echo 'a  b'"
    );
    // Metacharacters ride quoted too: unquoted, `bash -c` would
    // glob, expand, or split them on the reparse.
    assert_eq!(requote("*"), "'*'");
    assert_eq!(requote("$HOME"), "'$HOME'");
    assert_eq!(requote("it's"), "'it'\\''s'");
    assert_eq!(requote(""), "''");
}

#[cfg(unix)]
#[test]
fn a_quoted_argument_round_trips_through_the_backend_line() {
    // The listing shows the job the shim would have started, its
    // command carrying the quoted form: the same words ask again and
    // find their own row, so quoting survives the trip both ways.
    let quoted = listing(&entry_with_command(
        "abc",
        "running",
        "[s1] echo 'a  b'",
        "echo 'a  b'",
    ));
    let fake = Fake::new(&quoted);
    let argv = vec![
        OsString::from("job"),
        OsString::from("run"),
        OsString::from("echo"),
        OsString::from("a  b"),
    ];
    let code = call(&fake, &argv, Some("s1"));
    assert_eq!(code, 0);
    let calls = fake.calls();
    assert!(
        !calls.contains("add"),
        "the quoted line must dedupe against its own row: {calls}"
    );
}

#[cfg(unix)]
#[test]
fn a_stopped_job_does_not_block_a_new_one() {
    let fake = Fake::new(&listing(&entry(
        "abc",
        "stopped",
        "[s1] npm run dev",
        Some(0),
    )));
    let argv = vec![
        OsString::from("job"),
        OsString::from("run"),
        OsString::from("npm"),
        OsString::from("run"),
        OsString::from("dev"),
    ];
    assert_eq!(call(&fake, &argv, Some("s1")), 0);
    assert!(fake.calls().contains("add"), "{}", fake.calls());
}

#[cfg(unix)]
#[test]
fn the_listing_is_the_sessions_own_unless_asked_for_all() {
    let fake = Fake::new(&listing(&format!(
        "{},{}",
        entry("abc", "running", "[s1] npm run dev", None),
        entry("def", "stopped", "[s2] cargo build", Some(0)),
    )));
    let mine = rows(&fake, Scope::Session, Some("s1"));
    assert_eq!(
        mine.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ["abc"]
    );
    let all = rows(&fake, Scope::Machine, Some("s1"));
    assert_eq!(
        all.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ["abc", "def"]
    );
}

#[cfg(unix)]
#[test]
fn a_job_started_outside_the_shim_is_nobodys() {
    let fake = Fake::new(&listing(&entry("abc", "running", "hand started", None)));
    let mine = rows(&fake, Scope::Session, Some("s1"));
    assert!(mine.is_empty());
    let all = rows(&fake, Scope::Machine, Some("s1"));
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].session, None);
    assert_eq!(all[0].command, "npm run dev");
}

#[cfg(unix)]
#[test]
fn shutdown_stops_only_this_sessions_running_jobs() {
    let fake = Fake::new(&listing(&format!(
        "{},{},{}",
        entry("abc", "running", "[s1] npm run dev", None),
        entry("def", "running", "[s2] cargo build", None),
        entry("ghi", "stopped", "[s1] cargo test", Some(0)),
    )));
    let code = shutdown_command(&fake.backend, fake.dir.path(), Some("s1"));
    assert_eq!(code, 0);
    let calls = fake.calls();
    assert!(calls.contains("stop abc"), "{calls}");
    assert!(!calls.contains("stop def"), "{calls}");
    assert!(!calls.contains("stop ghi"), "{calls}");
}

#[cfg(unix)]
#[test]
fn a_stop_records_the_id_for_its_session() {
    let fake = Fake::new(&listing(&entry(
        "abc",
        "running",
        "[sr1] npm run dev",
        None,
    )));
    let argv = vec![
        OsString::from("job"),
        OsString::from("stop"),
        OsString::from("abc"),
    ];
    assert_eq!(call(&fake, &argv, Some("sr1")), 0);
    let recorded = stopped_ids_in(&stopped_record_path("sr1"));
    assert!(recorded.contains("abc"), "the stop left no record");
    forget_stopped_in(&stopped_record_path("sr1"));
}

#[cfg(unix)]
#[test]
fn a_stop_the_backend_refused_is_not_recorded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state.json");
    std::fs::write(&state, "[]").expect("state");
    let backend = script(
        dir.path(),
        "gob",
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n  list) cat '{}'; exit 0;;\n  stop) exit 1;;\n  *) exit 0;;\nesac\n",
            state.display()
        ),
    );
    let argv = vec![
        OsString::from("job"),
        OsString::from("stop"),
        OsString::from("abc"),
    ];
    assert_eq!(call_with(&backend, dir.path(), &argv, Some("sr2")), 1);
    assert!(
        stopped_ids_in(&stopped_record_path("sr2")).is_empty(),
        "a refused stop recorded nothing"
    );
    forget_stopped_in(&stopped_record_path("sr2"));
}

#[cfg(unix)]
#[test]
fn shutdown_records_the_jobs_it_stopped() {
    let fake = Fake::new(&listing(&format!(
        "{},{}",
        entry("abc", "running", "[sr3] npm run dev", None),
        entry("def", "running", "[s2] cargo build", None),
    )));
    assert_eq!(
        shutdown_command(&fake.backend, fake.dir.path(), Some("sr3")),
        0
    );
    let recorded = stopped_ids_in(&stopped_record_path("sr3"));
    assert!(recorded.contains("abc"), "the shutdown left no record");
    assert!(
        !recorded.contains("def"),
        "another session's job recorded against this one"
    );
    forget_stopped_in(&stopped_record_path("sr3"));
}

#[cfg(unix)]
#[test]
fn an_absent_record_reads_as_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(
        stopped_ids_in(&dir.path().join("absent")).is_empty(),
        "no file, no ids"
    );
}

#[cfg(unix)]
fn call_with(backend: &Path, cwd: &Path, argv: &[OsString], session: Option<&str>) -> i32 {
    let mut code = EXIT_NO_BACKEND;
    for _ in 0..50 {
        code = run_cli(argv, backend, cwd, session);
        if code != EXIT_NO_BACKEND {
            return code;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    code
}

#[cfg(unix)]
#[test]
fn a_missing_session_is_not_a_wired_shell() {
    let fake = Fake::new(&listing(""));
    let argv = vec![
        OsString::from("job"),
        OsString::from("run"),
        OsString::from("npm"),
        OsString::from("run"),
        OsString::from("dev"),
    ];
    assert_eq!(call(&fake, &argv, None), EXIT_NOT_WIRED);
    assert!(fake.calls().is_empty(), "{}", fake.calls());
}

#[cfg(unix)]
#[test]
fn an_unparsed_start_line_falls_back_to_the_tagged_listing() {
    let fake = Fake::new(&listing(&entry("xyz", "running", "[s1] npm run dev", None)));
    assert_eq!(
        find_by_description(&fake.backend, fake.dir.path(), "[s1] npm run dev"),
        Some("xyz".to_owned())
    );
    assert_eq!(
        find_by_description(&fake.backend, fake.dir.path(), "[s1] other"),
        None
    );
}

#[cfg(unix)]
fn fake_bin(dir: &std::path::Path) -> PathBuf {
    script(dir, "rhema-fake", "#!/bin/sh\necho \"$@\"\n")
}

#[cfg(unix)]
#[test]
fn the_state_provision_writes_no_init_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = fake_bin(dir.path());
    let state = dir.path().join("state");
    let wiring = provision_in_dir(state, None, &bin, "no-init", false).expect("provisions");
    assert!(!wiring.init_path.exists(), "production writes no init");
    assert!(
        !wiring.shell_env.contains_key(VAR_BASH_ENV),
        "the kit placement names the one init file: {:?}",
        wiring.shell_env
    );
    assert_eq!(
        wiring.shell_env.get(VAR_BIN).map(String::as_str),
        Some(bin.to_string_lossy().as_ref())
    );
}

#[cfg(unix)]
#[test]
fn the_init_file_defines_the_function_in_a_noninteractive_shell() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = fake_bin(dir.path());
    let wiring = provision_with_bin(None, &bin, "s9").expect("wiring");
    let out = std::process::Command::new("bash")
        .env(VAR_BASH_ENV, &wiring.shell_env[VAR_BASH_ENV])
        .env(VAR_BIN, &wiring.shell_env[VAR_BIN])
        .env(VAR_SESSION, &wiring.shell_env[VAR_SESSION])
        .arg("-c")
        .arg("job run 'echo hi'")
        .output()
        .expect("bash");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    // The fake binary prints its argv, so this is the function's forward
    // and nothing more.
    assert!(text.contains("__jobs run echo hi"), "{text}");
}

#[cfg(unix)]
#[test]
fn a_predecessor_init_still_defines_its_own_function() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = fake_bin(dir.path());
    let previous = dir.path().join("previous.bash");
    std::fs::write(&previous, "previous_macro() { echo previous; }\n").expect("previous");
    let wiring = provision_with_bin(Some(previous.into_os_string()), &bin, "s9").expect("wiring");
    let out = std::process::Command::new("bash")
        .env(VAR_BASH_ENV, &wiring.shell_env[VAR_BASH_ENV])
        .env(VAR_BIN, &wiring.shell_env[VAR_BIN])
        .arg("-c")
        .arg("previous_macro && job ls")
        .output()
        .expect("bash");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("previous"), "{text}");
    assert!(text.contains("__jobs ls"), "{text}");
}

#[cfg(unix)]
#[test]
fn a_job_started_elsewhere_in_the_session_still_belongs_to_it() {
    // The tag is the ownership record, not the directory: a server a
    // `cd` started belongs to the strip that started it.
    let fake = Fake::new("[]");
    let elsewhere = fake.dir.path().join("sub");
    std::fs::create_dir_all(&elsewhere).expect("sub");
    fake.set_listing(&listing(&entry_in(
        "abc",
        "running",
        "[s1] npm run dev",
        None,
        Some(elsewhere.to_str().expect("utf8 path")),
    )));
    let mine = rows(&fake, Scope::Session, Some("s1"));
    assert_eq!(
        mine.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ["abc"]
    );
}

#[cfg(unix)]
#[test]
fn the_same_line_in_another_directory_is_a_different_job() {
    // The backend dedupes by argv AND working directory, so a session
    // starting the same command in its own directory must not be told
    // about a job running somewhere else.
    let fake = Fake::new("[]");
    fake.set_listing(&listing(&entry_in(
        "abc",
        "running",
        "[s1] npm run dev",
        None,
        Some("/somewhere/else"),
    )));
    let argv = vec![
        OsString::from("job"),
        OsString::from("run"),
        OsString::from("npm"),
        OsString::from("run"),
        OsString::from("dev"),
    ];
    assert_eq!(call(&fake, &argv, Some("s2")), 0);
    let calls = fake.calls();
    assert!(calls.contains("add"), "the run was refused: {calls}");
}

#[cfg(unix)]
#[test]
fn the_init_file_is_exactly_the_forward_and_its_guard() {
    // The manual end-to-end runs reproduce this text byte for byte, and
    // the function is the model's whole interface: pin both.
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = fake_bin(dir.path());
    let wiring = provision_with_bin(None, &bin, "s9").expect("wiring");
    let text = std::fs::read_to_string(&wiring.init_path).expect("init");
    assert_eq!(
        text,
        format!(
            "# generated by rhema-bash; do not edit\n\
             {NAME}() {{ \"${VAR_BIN}\" {MODE} \"$@\"; }}\n"
        )
    );

    let previous = dir.path().join("previous.bash");
    std::fs::write(&previous, "").expect("previous");
    let chained =
        provision_with_bin(Some(previous.clone().into_os_string()), &bin, "s9").expect("wiring");
    let text = std::fs::read_to_string(&chained.init_path).expect("init");
    assert_eq!(
        text,
        format!(
            "# generated by rhema-bash; do not edit\n\
             source '{}' || true\n\
             {NAME}() {{ \"${VAR_BIN}\" {MODE} \"$@\"; }}\n",
            previous.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn an_absent_backend_is_not_a_fault() {
    // Nobody installing the backend gets a session without `job` and no
    // warning: the feature was never asked for.
    assert!(matches!(
        probe(Path::new("/nonexistent/gob")),
        Installed::Absent
    ));
}

#[cfg(unix)]
#[test]
fn a_backend_without_the_flag_is_installed_but_unusable() {
    // gob 1.2.3, which `go install ...@latest` fetches: it runs, it
    // starts jobs, and it cannot carry the owner tag.
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = script(
        dir.path(),
        "gob",
        "#!/bin/sh\necho 'Usage: gob add <command> [flags]'\necho '  -f, --follow   Follow output'\n",
    );
    match probe(&bin) {
        Installed::TooOld(reason) => {
            assert!(reason.contains("--description"), "{reason}");
            assert!(reason.contains("Homebrew"), "{reason}");
        }
        Installed::Absent | Installed::Ready => panic!("an old backend did not read as old"),
    }
}

#[cfg(unix)]
#[test]
fn a_backend_with_the_flag_is_ready() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = script(
        dir.path(),
        "gob",
        "#!/bin/sh\necho 'Usage: gob add <command> [flags]'\necho '      --description string   A note'\n",
    );
    assert!(matches!(probe(&bin), Installed::Ready));
}

/// A record carrying a job payload, the shape `reconcile` hands a kind.
fn job_record(tag: &str, id: &str, command: &str) -> rhema_submissions::Record {
    rhema_submissions::Record {
        id: rhema_submissions::SubmissionId::new("0000000000000001-00").unwrap(),
        kind: rhema_submissions::KindName::new(kind::NAME).unwrap(),
        payload: serde_json::json!({ "tag": tag, "id": id, "command": command }),
        status: rhema_submissions::Status::Running,
        created_at: 1,
        started_at: Some(2),
        settled_at: None,
        owner: None,
        outcome: None,
        attempts: 1,
    }
}

/// A listing entry carrying an explicit tag and command.
#[cfg(unix)]
fn tagged_entry(id: &str, status: &str, tag: &str, exit_code: Option<i32>) -> String {
    let exit = exit_code.map_or_else(|| "null".to_owned(), |code| code.to_string());
    format!(
        "{{\"id\":\"{id}\",\"status\":\"{status}\",\"command\":[\"bash\",\"-c\",\"npm run dev\"],\
         \"description\":\"[{tag}] npm run dev\",\"exit_code\":{exit},\
         \"workdir\":null,\"started_at\":\"2026-11-11T11:20:38+01:00\"}}"
    )
}

#[cfg(unix)]
#[test]
fn a_job_row_becomes_a_verdict() {
    use rhema_submissions::{Kind, RecoverCtx, Recovery};

    let fake = Fake::new(&listing(&tagged_entry("abc", "running", "s1", None)));
    let kind = kind::JobKind::at("s1", fake.backend.clone(), fake.dir.path().to_path_buf());
    let session = rhema_core::Session::default();
    let record = job_record("s1", "abc", "npm run dev");
    assert_eq!(
        kind.recover(&RecoverCtx {
            session: &session,
            record: &record,
        }),
        Recovery::Interrupted
    );

    fake.set_listing(&listing(&tagged_entry("abc", "stopped", "s1", Some(0))));
    assert_eq!(
        kind.recover(&RecoverCtx {
            session: &session,
            record: &record,
        }),
        Recovery::Done
    );

    // A reaped row is a finish, the live feed's own reading of an
    // absence.
    fake.set_listing("[]");
    assert_eq!(
        kind.recover(&RecoverCtx {
            session: &session,
            record: &record,
        }),
        Recovery::Done
    );
}

#[cfg(unix)]
#[test]
fn a_missing_backend_settles_the_record_failed() {
    use rhema_submissions::{Kind, RecoverCtx, Recovery};

    let dir = tempfile::tempdir().unwrap();
    let kind = kind::JobKind::at(
        "s1",
        dir.path().join("no-such-backend"),
        dir.path().to_path_buf(),
    );
    let session = rhema_core::Session::default();
    let record = job_record("s1", "abc", "npm run dev");
    assert_eq!(
        kind.recover(&RecoverCtx {
            session: &session,
            record: &record,
        }),
        Recovery::Failed {
            error: "no job backend".to_owned(),
        }
    );
}

#[cfg(unix)]
#[test]
fn adopt_rebuilds_the_running_records_that_are_missing() {
    use rhema_submissions::{Kinds, Status, reconcile, snapshot};

    let fake = Fake::new(&listing(&format!(
        "{},{},{}",
        tagged_entry("abc", "stopped", "s1", Some(0)),
        tagged_entry("def", "running", "s1", None),
        tagged_entry("xyz", "running", "s2", None)
    )));
    let kind = kind::JobKind::at("s1", fake.backend.clone(), fake.dir.path().to_path_buf());
    let mut session = rhema_core::Session::default();
    let kinds = Kinds::new().with(std::sync::Arc::new(kind));

    // The running row nobody tracks is adopted; the stopped row and
    // the other session's job are not. A stopped row is history the
    // live feed's rule mints nothing for, and a fresh `Running`
    // record of it would only settle into a phantom finish announced
    // at resume, the bug this pins down.
    let changed = reconcile(&mut session, &kinds, 10).unwrap();
    // The adopted record's own settle stays `Interrupted` (re-attached,
    // unannounced), never `Done`: only the fresh running row adopted.
    assert!(
        !changed.iter().any(|change| matches!(
            change,
            rhema_submissions::Change::Recovered {
                to: Status::Done,
                ..
            }
        )),
        "history minted a phantom finish: {changed:?}"
    );
    let after = snapshot(&session).unwrap();
    assert_eq!(after.len(), 1);
    let record = after.newest().unwrap();
    assert_eq!(record.status, Status::Interrupted);
    let payload: kind::Payload = serde_json::from_value(record.payload.clone()).unwrap();
    assert_eq!(payload.tag, "s1");
    assert_eq!(payload.id, "def");
    assert_eq!(payload.command, "npm run dev");

    // A second pass adopts nothing.
    assert!(reconcile(&mut session, &kinds, 11).unwrap().is_empty());
    assert_eq!(snapshot(&session).unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn a_stopped_row_nobody_tracked_is_not_adopted() {
    use rhema_submissions::{Kinds, reconcile, snapshot};

    let fake = Fake::new(&listing(&format!(
        "{},{}",
        tagged_entry("abc", "stopped", "s1", Some(1)),
        tagged_entry("def", "stopped", "s1", Some(0))
    )));
    let kind = kind::JobKind::at("s1", fake.backend.clone(), fake.dir.path().to_path_buf());
    let mut session = rhema_core::Session::default();
    let kinds = Kinds::new().with(std::sync::Arc::new(kind));

    // Both rows ended while nobody watched: history, not work to
    // index, and nothing recovers into an announced finish.
    let changed = reconcile(&mut session, &kinds, 10).unwrap();
    assert!(changed.is_empty(), "history minted a change: {changed:?}");
    assert!(snapshot(&session).unwrap().is_empty());
}

#[test]
fn a_retag_moves_the_tag_the_wired_shell_reads() {
    let wiring =
        provision_with_bin(None, Path::new("/bin/true"), "0001794000000000-00").expect("wiring");
    let file = wiring.session_path();
    // The first shell reads the tag the process provisioned.
    assert_eq!(
        session_tag(None, Some(&file)).as_deref(),
        Some("0001794000000000-00")
    );
    // A frontend that attaches another conversation rewrites the file,
    // and the shell's env map, frozen at provisioning time, follows.
    wiring.set_session("0001794000000001-00").expect("retag");
    assert_eq!(
        wiring.shell_env[VAR_SESSION], "0001794000000000-00",
        "the env map is the shell's, not the frontend's"
    );
    assert_eq!(
        session_tag(Some(wiring.shell_env[VAR_SESSION].clone()), Some(&file)).as_deref(),
        Some("0001794000000001-00")
    );
}

#[test]
fn a_wiring_without_a_tag_file_falls_back_to_the_env_var() {
    assert_eq!(
        session_tag(Some("s1".to_owned()), None).as_deref(),
        Some("s1")
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let empty = dir.path().join("session");
    std::fs::write(&empty, "").expect("empty");
    assert_eq!(
        session_tag(Some("s1".to_owned()), Some(&empty)).as_deref(),
        Some("s1"),
        "an empty tag file is no tag"
    );
    assert_eq!(session_tag(Some(String::new()), None), None);
}
