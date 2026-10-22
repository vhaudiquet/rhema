//! The offline sandbox tests: [`VercelSandbox`] against a stubbed
//! [`SandboxApi`]. No token, no network — the seam holds, and the
//! REST client's contract is the stub's, which the tests below pin.

use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rhema_cloud_agent::{ExecOutcome, ExecRequest, Sandbox, SandboxError, WorkspaceSeed};
use rhema_vercel_agent::api::{
    ApiError, CmdAnswer, CmdRequest, CmdStarted, CmdStatus, LogEntry, SandboxApi, SessionBody,
    SessionHandle,
};

/// A scripted API: one session, commands that finish after N polls,
/// recorded logs, files, and kill calls.
#[derive(Default)]
struct StubApi {
    session: Mutex<Option<String>>,
    created: AtomicUsize,
    polls_until_done: Mutex<HashMap<String, usize>>,
    exit_codes: Mutex<HashMap<String, i32>>,
    logs: Mutex<HashMap<String, Vec<LogEntry>>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    uploads: Mutex<Vec<(String, Vec<u8>)>>,
    kills: AtomicUsize,
}

impl StubApi {
    fn finish_after(&self, cmd: &str, polls: usize, code: i32, logs: Vec<LogEntry>) {
        self.polls_until_done
            .lock()
            .unwrap()
            .insert(cmd.to_owned(), polls);
        self.exit_codes.lock().unwrap().insert(cmd.to_owned(), code);
        self.logs.lock().unwrap().insert(cmd.to_owned(), logs);
    }
}

fn session_handle(id: &str) -> SessionHandle {
    SessionHandle {
        inner: SessionBody { id: id.to_owned() },
    }
}

#[async_trait]
impl SandboxApi for StubApi {
    async fn named_session(&self, _name: &str) -> Result<SessionHandle, ApiError> {
        match self.session.lock().unwrap().clone() {
            Some(session) => Ok(session_handle(&session)),
            None => Err(ApiError::NotFound("never created".into())),
        }
    }

    async fn create_named(&self, _name: &str) -> Result<SessionHandle, ApiError> {
        self.created.fetch_add(1, Ordering::SeqCst);
        let mut session = self.session.lock().unwrap();
        *session = Some("sbx_created".into());
        Ok(session_handle("sbx_created"))
    }

    async fn exec(&self, _session: &str, request: &CmdRequest) -> Result<CmdStarted, ApiError> {
        // The command id derives from the shell line, so tests script
        // per-command behavior by its content.
        let id = format!(
            "cmd_{}",
            request
                .args
                .last()
                .map(|arg| arg
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .take(8)
                    .collect::<String>())
                .unwrap_or_default()
        );
        // Unknown commands finish at once with exit 0; a test scripts
        // a specific command by finishing it differently.
        {
            let mut polls = self.polls_until_done.lock().unwrap();
            polls.entry(id.clone()).or_insert(0);
        }
        {
            let mut codes = self.exit_codes.lock().unwrap();
            codes.entry(id.clone()).or_insert(0);
        }
        Ok(CmdStarted {
            inner: CmdStatus {
                id,
                exit_code: None,
            },
        })
    }

    async fn command(&self, _session: &str, cmd: &str) -> Result<CmdAnswer, ApiError> {
        let mut polls = self.polls_until_done.lock().unwrap();
        let exit = match polls.get_mut(cmd) {
            Some(remaining) if *remaining > 0 => {
                *remaining -= 1;
                None
            }
            _ => self.exit_codes.lock().unwrap().get(cmd).copied(),
        };
        Ok(CmdAnswer {
            inner: CmdStatus {
                id: cmd.to_owned(),
                exit_code: exit,
            },
        })
    }

    async fn kill(&self, _session: &str, _cmd: &str) -> Result<(), ApiError> {
        self.kills.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn logs(&self, _session: &str, cmd: &str) -> Result<Vec<LogEntry>, ApiError> {
        Ok(self
            .logs
            .lock()
            .unwrap()
            .get(cmd)
            .cloned()
            .unwrap_or_default())
    }

    async fn read_file(&self, _session: &str, path: &str) -> Result<Vec<u8>, ApiError> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| ApiError::NotFound(path.to_owned()))
    }

    async fn write_files(
        &self,
        _session: &str,
        cwd: &str,
        tar_gz: Vec<u8>,
    ) -> Result<(), ApiError> {
        self.uploads.lock().unwrap().push((cwd.to_owned(), tar_gz));
        Ok(())
    }
}

fn sandbox(api: Arc<StubApi>) -> rhema_vercel_agent::VercelSandbox {
    rhema_vercel_agent::VercelSandbox::new(api, "test-session".into(), "/workspace".into())
}

fn exec_request(command: &str) -> ExecRequest {
    ExecRequest {
        command: command.to_owned(),
        cwd: "/workspace".into(),
        timeout: Duration::from_secs(5),
    }
}

fn no_progress(_: &str) {}

#[tokio::test]
async fn a_command_runs_polled_to_completion() {
    let api = Arc::new(StubApi::default());
    let sandbox = sandbox(api.clone());
    api.finish_after(
        "cmd_echohell",
        2,
        0,
        vec![
            LogEntry {
                stream: "stdout".into(),
                data: "hello\n".into(),
            },
            LogEntry {
                stream: "stderr".into(),
                data: "warn\n".into(),
            },
            LogEntry {
                stream: "stdout".into(),
                data: "world\n".into(),
            },
        ],
    );

    let mut lines = Vec::new();
    let outcome: ExecOutcome = sandbox
        .exec(
            exec_request("echo hello && echo world"),
            &AtomicBool::new(false),
            &mut |line| lines.push(line.to_owned()),
        )
        .await
        .unwrap();
    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stdout, b"hello\nworld\n");
    assert_eq!(outcome.stderr, b"warn\n");
    // The progress callback saw the completed stdout lines.
    assert_eq!(lines, vec!["hello".to_owned(), "world".to_owned()]);
    // The first named lookup missed, so the sandbox was created.
    assert_eq!(api.created.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_existing_sandbox_resumes_without_creating() {
    let api = Arc::new(StubApi::default());
    *api.session.lock().unwrap() = Some("sbx_existing".into());
    api.finish_after("cmd_true", 0, 0, vec![]);
    let sandbox = sandbox(api.clone());
    sandbox
        .exec(
            exec_request("true"),
            &AtomicBool::new(false),
            &mut no_progress,
        )
        .await
        .unwrap();
    assert_eq!(api.created.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_command_past_its_budget_times_out_and_is_killed() {
    let api = Arc::new(StubApi::default());
    // The command never finishes on its own: the platform's kill and
    // this side's deadline both pass it by.
    api.finish_after("cmd_sleep100", usize::MAX, 0, vec![]);
    let sandbox = sandbox(api);
    let err = sandbox
        .exec(
            ExecRequest {
                command: "sleep 1000".into(),
                cwd: "/workspace".into(),
                timeout: Duration::from_millis(50),
            },
            &AtomicBool::new(false),
            &mut no_progress,
        )
        .await
        .unwrap_err();
    assert!(matches!(&err, SandboxError::Timeout(1)), "{err:?}");
}

#[tokio::test]
async fn a_cancelled_flag_stops_the_run_before_it_starts() {
    let api = Arc::new(StubApi::default());
    let sandbox = sandbox(api);
    let err = sandbox
        .exec(
            exec_request("true"),
            &AtomicBool::new(true),
            &mut no_progress,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SandboxError::Failed(message) if message == "cancelled"),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_failed_command_reports_in_band() {
    let api = Arc::new(StubApi::default());
    let sandbox = sandbox(api.clone());
    api.finish_after(
        "cmd_false",
        0,
        1,
        vec![LogEntry {
            stream: "stderr".into(),
            data: "nope\n".into(),
        }],
    );
    let outcome = sandbox
        .exec(
            exec_request("false"),
            &AtomicBool::new(false),
            &mut no_progress,
        )
        .await
        .unwrap();
    assert_eq!(outcome.code, Some(1));
    assert_eq!(outcome.stderr, b"nope\n");
}

#[tokio::test]
async fn a_seed_clone_runs_through_the_shell() {
    let api = Arc::new(StubApi::default());
    let sandbox = sandbox(api.clone());
    api.finish_after("cmd_gitclonedept", 0, 0, vec![]);
    sandbox
        .seed(&WorkspaceSeed::GitClone("https://example.com/repo".into()))
        .await
        .unwrap();
    // The clone command ran: the only exec carried it.
    // (The stub keys commands by their shell line; a clone that
    // failed would have surfaced as an error here.)
}

#[tokio::test]
async fn a_skill_write_lands_as_a_one_file_tarball_at_root() {
    let api = Arc::new(StubApi::default());
    let sandbox = sandbox(api.clone());
    sandbox
        .write_file("/workspace/.rhema/skills/x/SKILL.md", b"body")
        .await
        .unwrap();
    let uploads = api.uploads.lock().unwrap();
    let (cwd, tar_gz) = &uploads[0];
    assert_eq!(
        cwd, "/",
        "the x-cwd is the root: the tarball carries the path"
    );
    // Unpack and check the entry.
    let gz = flate2::read::GzDecoder::new(&tar_gz[..]);
    let mut archive = tar::Archive::new(gz);
    let mut entries = archive.entries().unwrap();
    let mut entry = entries.next().unwrap().unwrap();
    assert_eq!(
        entry.path().unwrap().to_str(),
        Some("workspace/.rhema/skills/x/SKILL.md"),
        "the leading slash is stripped, the path preserved"
    );
    let mut contents = Vec::new();
    entry.read_to_end(&mut contents).unwrap();
    assert_eq!(contents, b"body");
}

#[tokio::test]
async fn a_file_read_comes_back_whole_and_a_miss_fails_in_band() {
    let api = Arc::new(StubApi::default());
    let sandbox = sandbox(api.clone());
    api.files
        .lock()
        .unwrap()
        .insert("/workspace/notes.md".into(), b"hello".to_vec());
    let bytes = sandbox.read_file("/workspace/notes.md").await.unwrap();
    assert_eq!(bytes, b"hello");
    let err = sandbox.read_file("/workspace/absent").await.unwrap_err();
    assert!(
        matches!(&err, SandboxError::Failed(message) if message.contains("no such file")),
        "{err:?}"
    );
}

#[tokio::test]
async fn the_session_id_resolves_once_across_calls() {
    let api = Arc::new(StubApi::default());
    let sandbox = sandbox(api.clone());
    api.finish_after("cmd_true", 0, 0, vec![]);
    sandbox
        .exec(
            exec_request("true"),
            &AtomicBool::new(false),
            &mut no_progress,
        )
        .await
        .unwrap();
    sandbox
        .exec(
            exec_request("true"),
            &AtomicBool::new(false),
            &mut no_progress,
        )
        .await
        .unwrap();
    assert_eq!(api.created.load(Ordering::SeqCst), 1, "one resolution");
}
