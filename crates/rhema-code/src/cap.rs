//! The session's host endpoint: one Unix socket where the sandbox's
//! `search` and `decide` shell functions hand their argv to the host.
//!
//! The endpoint is the host half of those two macros. The shell
//! function carries no key and no provider config; it sends the
//! request to this process, and the process resolves the profile's
//! providers and keys in memory. A backend that shares a path with the
//! host (local, Docker, LXD, ecrou) reaches the socket directly. SSH
//! asks the backend for a reverse socket forward. A backend with no
//! path to the host gets no endpoint, and its macros stay off.
//!
//! The wire is a tiny HTTP/1.1 exchange over one request per
//! connection: `POST /v1/search` or `POST /v1/decide` with a JSON body
//! `{"argv": [...], "stdin": "..."}`, answered with
//! `{"code": n, "stdout": "...", "stderr": "..."}`. The bearer token
//! scopes the socket to the session that provisioned it.

#![forbid(unsafe_code)]

#[cfg(unix)]
#[path = "cap/unix.rs"]
mod imp;
#[cfg(not(unix))]
#[path = "cap/other.rs"]
mod imp;

pub(crate) use imp::{Block, DecisionSetup, wire};
