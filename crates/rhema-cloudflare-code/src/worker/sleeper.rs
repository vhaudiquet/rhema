//! The retry pause over workerd's timers.
//!
//! tokio's time driver does not run inside workerd: there is no
//! Rust-side timer to fire. [`DelaySleeper`] implements the provider
//! crate's [`Sleeper`] over `worker::Delay`, which schedules a host
//! `setTimeout` — the same backoff ladder, a pause the runtime can
//! actually deliver.
#![forbid(unsafe_code)]

use std::time::Duration;

use async_trait::async_trait;
use rhema_provider::Sleeper;

/// The host-timer pause.
#[derive(Debug, Clone, Copy, Default)]
pub struct DelaySleeper;

#[async_trait]
impl Sleeper for DelaySleeper {
    async fn sleep(&self, duration: Duration) {
        // The `!Send` timer future runs on the local task queue; this
        // future only waits the plain one-shot back.
        let (done, fired) = futures::channel::oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            worker::Delay::from(duration).await;
            let _ = done.send(());
        });
        let _ = fired.await;
    }
}
