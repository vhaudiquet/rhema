//! The `Source` contract, end to end: a fake detection stream registered
//! through its own `name`/`key_fn` report, feeding a real dispatcher —
//! the fed count, the landed inbox, and the dedup on replay. Zero network.

mod common;

use std::sync::{Arc, Mutex};

use common::DEBOUNCE;
use rhema_autonomous_agent::async_trait;
use rhema_autonomous_agent::{Source, SourceError};
use rhema_rsd::{DispatchError, Dispatcher, Event, IngestResult, KeyFn, KeyState, ResourceKey};

/// A detection stream that replays a fixed feed into the dispatcher and
/// records what the dispatcher did with every event it was handed.
struct FakeSource {
    name: &'static str,
    dispatcher: Arc<Dispatcher>,
    feed: Mutex<Vec<Event>>,
    results: Mutex<Vec<IngestResult>>,
}

impl FakeSource {
    fn new(name: &'static str, dispatcher: Arc<Dispatcher>) -> Self {
        Self {
            name,
            dispatcher,
            feed: Mutex::new(Vec::new()),
            results: Mutex::new(Vec::new()),
        }
    }

    /// Queues one event for the next detect pass; the key function reads
    /// the resource key from `key`.
    fn feed(&self, key: &str, id: &str) {
        self.feed.lock().unwrap().push(Event {
            id: id.to_owned(),
            source: self.name.to_owned(),
            payload: serde_json::json!({ "key": key }),
            received_at: 0,
        });
    }

    fn recorded(&self) -> Vec<IngestResult> {
        self.results.lock().unwrap().clone()
    }
}

#[async_trait]
impl Source for FakeSource {
    fn name(&self) -> &str {
        self.name
    }

    fn key_fn(&self) -> Arc<KeyFn> {
        Arc::new(|event| ResourceKey::from(event.payload["key"].as_str().unwrap_or("unknown")))
    }

    async fn detect(&self) -> Result<usize, SourceError> {
        let feed = std::mem::take(&mut *self.feed.lock().unwrap());
        let mut fed = 0;
        for event in feed {
            let result = self.dispatcher.ingest(event)?;
            self.results.lock().unwrap().push(result);
            fed += 1;
        }
        Ok(fed)
    }
}

#[tokio::test]
async fn a_detect_pass_feeds_its_events_through_its_own_registration() {
    let (dispatcher, clock) = common::setup();
    let source = FakeSource::new("github", dispatcher.clone());

    // A consumer registers the source without reaching into its
    // implementation: the name and the key function are its whole report.
    dispatcher.register_source(source.name(), source.key_fn());

    source.feed("github:acme/widgets#142", "e1");
    source.feed("github:acme/widgets#142", "e2");
    source.feed("github:acme/widgets#7", "e3");
    assert_eq!(
        source.detect().await.unwrap(),
        3,
        "the pass counts every event it fed"
    );

    let key = ResourceKey::from("github:acme/widgets#142");
    let other = ResourceKey::from("github:acme/widgets#7");
    let status = dispatcher.status(&key).unwrap();
    assert_eq!(
        status.inbox_depth, 2,
        "ingest landed, key resolution via key_fn"
    );
    assert_eq!(status.state, KeyState::Idle);
    assert!(
        status.debounce_deadline.is_some(),
        "detection opened the debounce window"
    );
    assert_eq!(dispatcher.status(&other).unwrap().inbox_depth, 1);
    assert!(
        dispatcher.list_active().unwrap().is_empty(),
        "detection never dispatches"
    );

    clock.advance(DEBOUNCE);
    assert_eq!(
        dispatcher.admit().unwrap().len(),
        2,
        "both keys are dispatch-eligible once the window elapses"
    );

    // The replayed poll: same dedup ids, fed again, dropped as duplicates —
    // the inbox the driver will see is unchanged.
    source.feed("github:acme/widgets#142", "e1");
    source.feed("github:acme/widgets#142", "e2");
    source.feed("github:acme/widgets#7", "e3");
    assert_eq!(
        source.detect().await.unwrap(),
        3,
        "replays count as fed; the dispatcher dedups"
    );
    assert_eq!(dispatcher.status(&key).unwrap().inbox_depth, 2);
    assert_eq!(dispatcher.status(&other).unwrap().inbox_depth, 1);
    let recorded = source.recorded();
    assert!(
        recorded[..3]
            .iter()
            .all(|r| *r == IngestResult::DebouncePending)
    );
    assert!(recorded[3..].iter().all(|r| *r == IngestResult::Duplicate));
}

#[tokio::test]
async fn a_detect_pass_at_an_unregistered_source_fails_in_band() {
    let (dispatcher, _clock) = common::setup();
    let source = FakeSource::new("ghost", dispatcher.clone());
    source.feed("github:acme/widgets#142", "e1");
    assert!(matches!(
        source.detect().await.unwrap_err(),
        SourceError::Dispatch(DispatchError::UnknownSource(_))
    ));
}
