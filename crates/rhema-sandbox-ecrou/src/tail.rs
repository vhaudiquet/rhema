//! The capture files tailed while the machine runs: the offset, the
//! capped tail, and the exit's one reading, ecrou's own words apart
//! from the guest's.

use rhema_sandbox::RunOutcome;
use rhema_sandbox::SandboxError;
use rhema_sandbox::ToolProgress;
use rhema_sandbox::exec_util::EXEC_TAIL_BYTES;

/// One capture file, tailed: the read offset, and the capped tail of
/// the complete lines seen so far.
#[derive(Debug, Default)]
pub(crate) struct StreamTail {
    pub(crate) path: std::path::PathBuf,
    pub(crate) pos: u64,
    pub(crate) tail: Vec<u8>,
}

impl StreamTail {
    /// Reads what appeared since the last tick, emits every complete
    /// line through `progress`, and keeps the capped tail. `final_pass`
    ///, the drain after the machine is gone, also consumes a
    /// trailing partial line as a line, the way the pipe pump's
    /// `lines()` yields it at end of stream; mid-run, a partial line
    /// waits for its newline. The file does not exist until the
    /// guest's first write, absence is silence, not an error.
    pub(crate) async fn pump(
        &mut self,
        progress: &mut dyn ToolProgress,
        final_pass: bool,
    ) -> Result<(), SandboxError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt as _};

        let Ok(mut file) = tokio::fs::File::open(&self.path).await else {
            // The guest creates the file on its first write; a boot
            // that dies early leaves it absent for the whole call.
            return Ok(());
        };
        let len = file
            .metadata()
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{}: {err}", self.path.display())))?
            .len();
        if len <= self.pos {
            return Ok(());
        }
        file.seek(std::io::SeekFrom::Start(self.pos))
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{}: {err}", self.path.display())))?;
        let mut bytes = Vec::with_capacity((len - self.pos) as usize);
        file.read_to_end(&mut bytes)
            .await
            .map_err(|err| SandboxError::Unavailable(format!("{}: {err}", self.path.display())))?;
        let complete = match bytes.iter().rposition(|b| *b == b'\n') {
            Some(at) => &bytes[..=at],
            // No complete line: a final pass takes the partial tail as
            // a line, a mid-run tick waits for the newline and reads
            // the whole partial line again next tick.
            None if !final_pass => return Ok(()),
            None => bytes.as_slice(),
        };
        self.pos += complete.len() as u64;
        if complete.last() != Some(&b'\n') {
            let line = String::from_utf8_lossy(complete).into_owned();
            progress.output(&format!("{line}\n"));
            append_capped(&mut self.tail, format!("{line}\n").as_bytes());
        } else {
            for line in complete.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                progress.output(&format!("{}\n", String::from_utf8_lossy(line)));
            }
            append_capped(&mut self.tail, complete);
        }
        Ok(())
    }

    /// The capped tail, the pump's.
    pub(crate) fn take(self) -> Vec<u8> {
        self.tail
    }
}

/// Appends `bytes` to `tail`, keeping at most the last
/// [`EXEC_TAIL_BYTES`], the same cap the pipe pump applies as the
/// bytes land.
pub(crate) fn append_capped(tail: &mut Vec<u8>, bytes: &[u8]) {
    tail.extend_from_slice(bytes);
    if tail.len() > EXEC_TAIL_BYTES {
        let excess = tail.len() - EXEC_TAIL_BYTES;
        tail.drain(..excess);
    }
}

/// The stderr lines that are ecrou's own words, not the guest's: the
/// runtime's `Error:` diagnostics and clap's lowercase `error:` usage
/// complaints. The payload's streams never touch this stderr, they
/// ride the capture files, so a warning is the only thing that may
/// appear next to a guest's own nonzero exit.
fn is_ecrou_error(line: &str) -> bool {
    line.starts_with("Error: ") || line.starts_with("error: ")
}

/// Reads the ecrou process's stderr and sorts the exit out: a line of
/// ecrou's own next to a nonzero exit is a failed boot, an image that
/// would not resolve, an old binary, an unavailable sandbox, never a
/// guest result. Warnings ride along on successes and on guest
/// failures alike; a guest exit is the command's own result.
pub(crate) fn classify(
    code: Option<i32>,
    ecrou_stderr: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<RunOutcome, SandboxError> {
    let failed = code != Some(0) && ecrou_stderr.lines().any(is_ecrou_error);
    if failed {
        let detail = ecrou_stderr
            .lines()
            .rev()
            .find(|line| is_ecrou_error(line))
            .map(|line| {
                line.trim_start_matches("Error: ")
                    .trim_start_matches("error: ")
                    .to_owned()
            })
            .unwrap_or_else(|| ecrou_stderr.trim().to_owned());
        return Err(SandboxError::Unavailable(detail));
    }
    Ok(RunOutcome {
        stdout,
        stderr,
        code,
    })
}
