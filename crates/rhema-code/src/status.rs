//! Program status over OSC 7501: the escape sequence that tells the
//! terminal what rhema is doing — working, blocked on the user, idle,
//! done, failed — so tab headers, session pickers, and agentic inboxes
//! can show it without scraping the screen. See Mitchell Hashimoto's
//! proposal (mitchellh.com/writing/program-status-osc7501). Terminals
//! must ignore OSC sequences they do not know, so emission needs no
//! feature detection; the only gate is a terminal being attached to
//! stdout at all.
//!
//! Wire shape: `ESC ] 7501 ; key=value:key=value ESC \`. The value of
//! `msg` is base64 UTF-8, capped at the spec's 2048 decoded bytes; the
//! other values are plain. One report fully replaces the record it
//! addresses, and rhema addresses a single root record — one agent per
//! terminal, so the spec's hierarchical `id`s have nothing to address.
//!
//! TERMINAL OWNERSHIP: emissions happen on the UI thread — through the
//! [`Tracker`] the event loop syncs — or in the headless runner, which
//! owns its stdout. The turn task reports through the event channel and
//! never writes. The tracker derives the state from those events and the
//! app, and re-emits only when the rendered report changed.
//!
//! LIFETIME: the TUI removes its record when it leaves the terminal — an
//! exited interactive session is no status at all. The headless one-shot
//! never removes it: `done` or `error` standing after exit is the point.

use std::io::{IsTerminal, Write};

use crate::app::App;
use crate::events::{OwnedEvent, UiEvent};

/// One program-status report.
#[derive(Debug)]
pub(crate) enum Status {
    /// At rest at the prompt, awaiting the user.
    Idle,
    /// A turn is running. `progress` is the step fraction when the
    /// turn's step cap gives it meaning; `msg` names the intent — the
    /// prompt until the first tool call, the latest tool call after.
    Working {
        /// Human-readable line; base64 on the wire.
        msg: Option<String>,
        /// 0–100, when the step cap is known.
        progress: Option<u8>,
    },
    /// The turn is parked on the user.
    Blocked {
        /// What kind of attention it needs.
        kind: Kind,
        /// Human-readable line; base64 on the wire.
        msg: Option<String>,
    },
    /// Finished with the result unseen — the headless one-shot's exit
    /// state, left standing so the terminal can mark the tab.
    Done,
    /// The turn failed; the session is back at the prompt.
    Error {
        /// First line of the failure; base64 on the wire.
        msg: Option<String>,
    },
}

/// Why a [`Status::Blocked`] turn blocks.
#[derive(Debug)]
pub(crate) enum Kind {
    /// A gated tool call awaits the user's answer.
    Permission,
    /// An interactive login flow awaits the user's browser.
    Auth,
}

impl Kind {
    /// The wire spelling the spec defines.
    fn as_str(&self) -> &'static str {
        match self {
            Kind::Permission => "permission",
            Kind::Auth => "auth",
        }
    }
}

/// The bytes that remove rhema's record, for terminal teardown.
const CLEAR: &[u8] = b"\x1b]7501;state=clear\x1b\\";

/// The full escape sequence of one report, terminated by ST.
fn sequence(status: &Status) -> String {
    format!("\x1b]7501;{}\x1b\\", body(status))
}

/// The report body: `key=value` pairs joined by colons, in the order of
/// the spec's own example. Each report carries `app`, the stable
/// identifier inboxes match against.
fn body(status: &Status) -> String {
    let mut parts: Vec<String> = Vec::new();
    match status {
        Status::Idle => parts.push("state=idle".to_owned()),
        Status::Working { progress, .. } => {
            parts.push("state=working".to_owned());
            if let Some(progress) = progress {
                parts.push(format!("progress={progress}"));
            }
        }
        Status::Blocked { kind, .. } => {
            parts.push("state=blocked".to_owned());
            parts.push(format!("kind={}", kind.as_str()));
        }
        Status::Done => parts.push("state=done".to_owned()),
        Status::Error { .. } => parts.push("state=error".to_owned()),
    }
    parts.push("app=rhema".to_owned());
    if let Some(msg) = msg_of(status) {
        parts.push(format!("msg={}", base64_of(capped(msg))));
    }
    parts.join(":")
}

/// The human-readable line a report carries, if any.
fn msg_of(status: &Status) -> Option<&str> {
    match status {
        Status::Working { msg: Some(msg), .. }
        | Status::Blocked { msg: Some(msg), .. }
        | Status::Error { msg: Some(msg) } => Some(msg),
        _ => None,
    }
}

/// Standard base64 (RFC 4648, with padding), the encoding the spec
/// requires of `msg`. Written here rather than pulled in as a
/// dependency: a few one-line strings per turn do not justify one.
fn base64_of(data: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = data.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let pad = 3 - chunk.len();
        for shift in [18, 12, 6, 0] {
            let sextet = (n >> shift) & 0b11_1111;
            let symbol = if (shift == 6 && pad == 2) || (shift == 0 && pad >= 1) {
                '='
            } else {
                ALPHABET[sextet as usize] as char
            };
            out.push(symbol);
        }
    }
    out
}

/// The spec caps one decoded `msg` at 2048 bytes; enforce the cap
/// without splitting a character. Our messages are one short line, so
/// this is a guard, not a routine path.
fn capped(msg: &str) -> &str {
    const MAX_DECODED: usize = 2048;
    if msg.len() <= MAX_DECODED {
        return msg;
    }
    let mut end = MAX_DECODED;
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    &msg[..end]
}

/// Step `step` of `max_steps` as the 0–100 percentage the spec's
/// `progress` takes. A turn may run more steps than the cap allows —
/// the cap is a limit, not a promise — so the figure saturates at 100.
pub(crate) fn progress(step: u32, max_steps: u32) -> u8 {
    let max = max_steps.max(1);
    (step.saturating_mul(100) / max).min(100) as u8
}

/// First line of a multi-line text, cut for a one-line display: the
/// status `msg` and the headless stderr stream preview.
pub(crate) fn first_line(content: &str) -> String {
    let line = content.lines().next().unwrap_or("");
    let mut cut = line.chars().take(72).collect::<String>();
    if line.chars().count() > 72 {
        cut.push('…');
    }
    cut
}

/// Writes one report to stdout, when a terminal is attached there. A
/// piped stdout captures the reply text byte-exact — the sequence would
/// be pollution there, and no terminal would see it anyway. Errors are
/// ignored: a failed status write must never fail the session.
pub(crate) fn emit(status: &Status) {
    let mut out = std::io::stdout().lock();
    if !out.is_terminal() {
        return;
    }
    let _ = out.write_all(sequence(status).as_bytes());
    let _ = out.flush();
}

/// Removes rhema's record. The TUI calls this on terminal teardown, so
/// an exited session never leaves a stale badge behind; the headless
/// runner does not — its `done`/`error` is what survives it.
pub(crate) fn clear() {
    let mut out = std::io::stdout().lock();
    if !out.is_terminal() {
        return;
    }
    let _ = out.write_all(CLEAR);
    let _ = out.flush();
}

/// Derives the report for the TUI and remembers what was emitted. Lives
/// on the UI thread with the event loop: `observe` folds the engine
/// events in as they drain, `sync` derives from the app and emits on
/// change. The turn's outcome is tracked here rather than read from the
/// app's `error` — that one deliberately outlives a turn ("until the
/// next submit"), while the record must not: a turn that ends well is
/// `idle`, whatever the last failure was.
#[derive(Default)]
pub(crate) struct Tracker {
    /// The body last emitted, to skip re-emitting an unchanged report.
    last: Option<String>,
    /// The intent of the running turn: the prompt's first line until a
    /// tool call replaces it.
    msg: Option<String>,
    /// The running turn's last announced step, for `progress`.
    step: Option<u32>,
    /// First line of the turn that failed last, until a turn ends
    /// otherwise.
    failed: Option<String>,
}

impl Tracker {
    /// Notes the prompt a turn starts on: the `msg` until the first
    /// tool call names a more specific one.
    pub(crate) fn note_prompt(&mut self, prompt: &str) {
        self.msg = Some(first_line(prompt));
    }

    /// Folds one UI event into the working-state detail.
    pub(crate) fn observe(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Engine(OwnedEvent::StepStart { iteration }) => self.step = Some(*iteration),
            UiEvent::Engine(OwnedEvent::ToolCallStart { name, .. }) => {
                self.msg = Some(name.clone());
            }
            UiEvent::TurnFinished { result, .. } => {
                self.msg = None;
                self.step = None;
                self.failed = match result {
                    Ok(_) => None,
                    Err(message) => Some(first_line(message)),
                };
            }
            UiEvent::TurnCancelled { .. } => {
                self.msg = None;
                self.step = None;
                self.failed = None;
            }
            _ => {}
        }
    }

    /// The state the app is in, by precedence: a pending approval and a
    /// login flow block on the user, a turn runs, a failed turn reads as
    /// an error at the prompt, everything else is idle.
    pub(crate) fn status_for(&self, app: &App) -> Status {
        if let Some(call) = &app.turn.pending_approval {
            return Status::Blocked {
                kind: Kind::Permission,
                msg: Some(call.name.clone()),
            };
        }
        if app.login_cancel.is_some() {
            return Status::Blocked {
                kind: Kind::Auth,
                msg: None,
            };
        }
        if app.turn.running {
            return Status::Working {
                msg: self.msg.clone(),
                progress: self.step.map(|step| progress(step, app.options.max_steps)),
            };
        }
        match &self.failed {
            Some(first) => Status::Error {
                msg: Some(first.clone()),
            },
            None => Status::Idle,
        }
    }

    /// Derives the report from the app and emits it when it changed.
    /// Once per loop iteration is enough: transitions the app makes
    /// mid-iteration surface with the next one, well under the poll
    /// interval.
    pub(crate) fn sync(&mut self, app: &App) {
        let status = self.status_for(app);
        let body = body(&status);
        if self.last.as_deref() != Some(body.as_str()) {
            self.last = Some(body);
            emit(&status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_reports_its_state_and_app() {
        assert_eq!(
            sequence(&Status::Idle),
            "\x1b]7501;state=idle:app=rhema\x1b\\"
        );
    }

    #[test]
    fn working_carries_progress_and_a_base64_msg() {
        assert_eq!(
            sequence(&Status::Working {
                msg: Some("bash".to_owned()),
                progress: Some(40),
            }),
            "\x1b]7501;state=working:progress=40:app=rhema:msg=YmFzaA==\x1b\\"
        );
    }

    #[test]
    fn blocked_names_its_kind() {
        assert_eq!(
            sequence(&Status::Blocked {
                kind: Kind::Permission,
                msg: None,
            }),
            "\x1b]7501;state=blocked:kind=permission:app=rhema\x1b\\"
        );
        assert_eq!(
            sequence(&Status::Blocked {
                kind: Kind::Auth,
                msg: None,
            }),
            "\x1b]7501;state=blocked:kind=auth:app=rhema\x1b\\"
        );
    }

    #[test]
    fn done_and_error_report_their_states() {
        assert_eq!(
            sequence(&Status::Done),
            "\x1b]7501;state=done:app=rhema\x1b\\"
        );
        assert_eq!(
            sequence(&Status::Error {
                msg: Some("no".to_owned()),
            }),
            "\x1b]7501;state=error:app=rhema:msg=bm8=\x1b\\"
        );
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        assert_eq!(base64_of(""), "");
        assert_eq!(base64_of("f"), "Zg==");
        assert_eq!(base64_of("fo"), "Zm8=");
        assert_eq!(base64_of("foo"), "Zm9v");
        assert_eq!(base64_of("foob"), "Zm9vYg==");
        assert_eq!(base64_of("fooba"), "Zm9vYmE=");
        assert_eq!(base64_of("foobar"), "Zm9vYmFy");
        // Multibyte UTF-8 rides whole: é is bytes C3 A9.
        assert_eq!(base64_of("é"), "w6k=");
    }

    #[test]
    fn a_long_msg_is_cut_at_the_spec_cap_whole_characters() {
        let msg = "é".repeat(2048);
        let cut = capped(&msg);
        assert_eq!(cut.len(), 2048);
        assert!(cut.ends_with('é'));
        // 2048 decoded bytes still fit the spec's 4096-byte report cap.
        assert!(
            body(&Status::Error {
                msg: Some(cut.to_owned()),
            })
            .len()
                < 4096
        );
    }

    #[test]
    fn progress_scales_the_step_fraction_and_saturates() {
        assert_eq!(progress(0, 10), 0);
        assert_eq!(progress(5, 10), 50);
        assert_eq!(progress(9, 10), 90);
        assert_eq!(progress(99, 10), 100);
        // A zero cap divides by one instead.
        assert_eq!(progress(0, 0), 0);
    }

    #[test]
    fn first_line_keeps_the_first_line_only() {
        assert_eq!(first_line("one\ntwo"), "one");
        let long = "x".repeat(80);
        let cut = first_line(&long);
        assert_eq!(cut.chars().count(), 73);
        assert!(cut.ends_with('…'));
    }
}
