//! The normalized event vocabulary shared by every forge backend.
//!
//! Three REST dialects reduce to one shape here. Constraints the
//! normalization pins (each backend module documents how it meets them):
//! - An event is a thread-level fact ([`EventKind::Opened`], `Reopened`,
//!   `Closed`, `Merged`), a comment ([`EventKind::Comment`]), or a
//!   submitted review ([`EventKind::Review`]). Whether the thread is an
//!   issue or a pull/merge request rides on [`ForgeEvent::is_pr`], so the
//!   kind list stays small and the "issue comment vs PR comment" split is
//!   `EventKind::Comment` + `is_pr`.
//! - [`ForgeEvent::id`] is the dedup id the RSD dispatcher keys on: the
//!   feed's item id plus, for thread-level rows, the row's updated time —
//!   stable while a row is unchanged, new the moment it changes, exactly
//!   the "thread's updated_at as dedup id" scheme of RSD §8. Comment and
//!   review rows are immutable enough that their own ids suffice.
//! - Thread-level polls read row snapshots, not transitions: the actor on
//!   such an event is the thread author, never the (unknowable from a
//!   poll) closer or merger.

use core::cmp::Ordering;
use core::fmt;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// What happened to a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The thread was created.
    Opened,
    /// A closed thread returned to open.
    Reopened,
    /// The thread was closed (a pull/merge request: closed unmerged).
    Closed,
    /// A pull/merge request was merged. Never an issue.
    Merged,
    /// A comment was posted; [`ForgeEvent::is_pr`] separates issue from
    /// pull-request comments.
    Comment,
    /// A formal review was submitted on a pull/merge request.
    Review,
}

/// The coarse state of a thread as it stands right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    /// Open for business.
    Open,
    /// Closed; a pull/merge request closed unmerged.
    Closed,
    /// A merged pull/merge request.
    Merged,
}

/// Repository coordinates. `owner` + `name` + a thread [`ForgeEvent::number`]
/// is the resource key the RSD dispatcher forms (`host:owner/repo#number`).
///
/// GitLab's `owner` is the whole namespace path (`group/subgroup`), so
/// [`RepoRef::path`] is always the full project path the forge addresses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// Owner login, or the namespace path on GitLab.
    pub owner: String,
    /// Repository or project slug.
    pub name: String,
}

impl RepoRef {
    /// Coordinates for `owner`/`name`. Neither is validated: callers pass
    /// forge-correct names (the adapters build these from forge data).
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// `owner/name` — the full project path.
    pub fn path(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// A thread's current state: everything a handler needs to orient without
/// a second API round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The thread title.
    pub title: String,
    /// The opening body, when the thread has one.
    pub body: Option<String>,
    /// Open, closed, or merged.
    pub state: ThreadState,
    /// Whether the thread is a pull/merge request.
    pub is_pr: bool,
    /// Who opened the thread.
    pub author: Option<String>,
    /// Label names, bare.
    pub labels: Vec<String>,
    /// Human-facing URL of the thread.
    pub url: Option<String>,
    /// The untouched wire row the snapshot came from.
    pub raw: serde_json::Value,
}

/// One normalized piece of forge activity: a thread transition, a comment,
/// or a submitted review.
///
/// The shape the RSD dispatcher consumes: [`ForgeEvent::repo`] plus
/// [`ForgeEvent::number`] form the resource key, [`ForgeEvent::id`] is the
/// dedup id, and [`ForgeEvent::raw`] carries the untouched wire row for
/// handler context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForgeEvent {
    /// Stable dedup id: the feed's item id plus, for thread-level rows,
    /// the row's updated time (e.g. `github:pull:1824@2026-10-08T12:00:00Z`,
    /// `github:comment:29482`). Two polls of the same unchanged row produce
    /// the same id; a changed row produces a new one.
    pub id: String,
    /// What happened.
    pub kind: EventKind,
    /// The repository the thread lives in.
    pub repo: RepoRef,
    /// The thread's number (GitLab: the iid).
    pub number: u64,
    /// Whether the thread is a pull/merge request rather than an issue.
    pub is_pr: bool,
    /// Who acted: the thread author on thread-level events (a poll sees row
    /// snapshots, not transitions, so the closer/merger is unknowable), the
    /// comment or review author otherwise.
    pub actor: Option<String>,
    /// The comment or review text; `None` on thread-level events (the
    /// thread body lives on the snapshot, not on the event).
    pub body: Option<String>,
    /// Human-facing URL of the event, when the wire row carries one.
    pub url: Option<String>,
    /// When the row was last touched (thread rows) or created/submitted
    /// (comments, reviews). The poll cursor advances to the newest of
    /// these.
    pub timestamp: Timestamp,
    /// The untouched wire row, minus the keys the event lifts.
    pub raw: serde_json::Value,
}

/// An RFC 3339 timestamp, kept in the forge's own spelling.
///
/// The raw string is what travels: it round-trips onto cursors and into
/// events unchanged. Ordering compares parsed instants, never text — the
/// forges disagree on fractional seconds, and `…:00Z` must sort before
/// `…:00.081Z`, which string comparison gets backwards.
#[derive(Debug, Clone)]
pub struct Timestamp {
    raw: String,
    /// Whole seconds since the Unix epoch (negative before 1970).
    secs: i64,
    /// Sub-second nanoseconds, `[0, 1_000_000_000)`.
    nanos: u32,
}

impl Timestamp {
    /// Parses an RFC 3339 timestamp: `YYYY-MM-DDThh:mm:ss[.frac]` followed
    /// by `Z` (either case) or a `±hh:mm` offset. `None` on anything else.
    ///
    /// Validation is structural, not calendrical (day-of-month is only
    /// bounds-checked): the forges are the source of truth and only the
    /// ordering key matters here.
    pub fn parse(raw: &str) -> Option<Self> {
        let bytes = raw.as_bytes();
        // `YYYY-MM-DDThh:mm:ss` is the 19-char floor; an offset must follow.
        if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        if bytes[10] != b'T' && bytes[10] != b't' {
            return None;
        }
        if bytes[13] != b':' || bytes[16] != b':' {
            return None;
        }
        let year = digits(raw, 0..4)?;
        let month = digits(raw, 5..7)?;
        let day = digits(raw, 8..10)?;
        let hour = digits(raw, 11..13)?;
        let minute = digits(raw, 14..16)?;
        let second = digits(raw, 17..19)?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        // 60 accepts the leap seconds RFC 3339 allows.
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }

        let mut index = 19;
        let mut nanos: u32 = 0;
        if bytes[index] == b'.' {
            index += 1;
            let start = index;
            while bytes.get(index).is_some_and(|b| b.is_ascii_digit()) {
                index += 1;
            }
            if index == start {
                return None;
            }
            // Truncate past nanosecond precision, pad short fractions.
            let mut fraction = raw[start..index].to_owned();
            fraction.truncate(9);
            while fraction.len() < 9 {
                fraction.push('0');
            }
            nanos = fraction.parse().ok()?;
        }

        let offset: i64 = match bytes.get(index) {
            Some(b'Z') | Some(b'z') => {
                index += 1;
                0
            }
            Some(sign @ (b'+' | b'-')) => {
                index += 1;
                if bytes.get(index + 2) != Some(&b':') {
                    return None;
                }
                let hours = digits(raw, index..index + 2)?;
                let minutes = digits(raw, index + 3..index + 5)?;
                if hours > 23 || minutes > 59 {
                    return None;
                }
                index += 5;
                let total = hours * 3600 + minutes * 60;
                if *sign == b'+' { total } else { -total }
            }
            _ => return None,
        };
        if index != bytes.len() {
            return None; // trailing garbage
        }

        let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - offset;
        Some(Self {
            raw: raw.to_owned(),
            secs,
            nanos,
        })
    }

    /// The raw string, in the forge's own spelling — the form cursors and
    /// `since`-style query parameters carry.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Whole seconds since the Unix epoch.
    pub fn epoch_secs(&self) -> i64 {
        self.secs
    }
}

/// A run of ASCII digits at `range`, as its number.
fn digits(raw: &str, range: core::ops::Range<usize>) -> Option<i64> {
    let slice = raw.get(range)?;
    if slice.is_empty() || !slice.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    slice.parse().ok()
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl PartialEq for Timestamp {
    fn eq(&self, other: &Self) -> bool {
        self.secs == other.secs && self.nanos == other.nanos
    }
}
impl Eq for Timestamp {}

impl PartialOrd for Timestamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Timestamp {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.secs, self.nanos).cmp(&(other.secs, other.nanos))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Timestamp::parse(&raw)
            .ok_or_else(|| de::Error::custom(format!("not an RFC 3339 timestamp: {raw:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_shapes_the_forges_emit() {
        // GitHub's shape.
        let zulu = Timestamp::parse("2026-10-08T00:00:00Z").expect("zulu parses");
        assert_eq!(zulu.epoch_secs(), 1_791_417_600);
        // GitLab's shape: fractional seconds.
        let millis = Timestamp::parse("2026-10-08T00:00:00.081Z").expect("millis parse");
        assert_eq!(millis.epoch_secs(), zulu.epoch_secs());
        assert!(millis > zulu);
        // Offsets compare by instant, not spelling.
        let shifted = Timestamp::parse("2026-10-08T02:00:00+02:00").expect("offset parse");
        assert_eq!(shifted, zulu);
        // RFC 3339 allows the lowercase forms.
        assert_eq!(
            Timestamp::parse("1970-01-01t00:00:00z")
                .expect("lowercase parse")
                .epoch_secs(),
            0
        );
        assert_eq!(
            Timestamp::parse("2016-01-04T15:31:51.081Z")
                .expect("gitlab fixture parse")
                .epoch_secs(),
            1_451_921_511
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for raw in [
            "",
            "2026-10-08",
            "2026-10-08T00:00:00",
            "2026-10-08 00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-10-32T00:00:00Z",
            "2026-10-08T24:00:00Z",
            "2026-10-08T00:60:00Z",
            "2026-10-08T00:00:00Z ",
            "2026-10-08T00:00:00+0200",
            "2026-10-08T00:00:00.Z",
            "2026-10-08T00:00:00Q",
        ] {
            assert!(Timestamp::parse(raw).is_none(), "for {raw:?}");
        }
    }

    #[test]
    fn serde_keeps_the_raw_string() {
        let ts = Timestamp::parse("2026-10-08T00:00:00.081Z").expect("parses");
        let encoded = serde_json::to_value(&ts).expect("serializes");
        assert_eq!(encoded, json!("2026-10-08T00:00:00.081Z"));
        assert_eq!(
            serde_json::from_value::<Timestamp>(encoded).expect("deserializes"),
            ts
        );
        assert!(serde_json::from_value::<Timestamp>(json!("nonsense")).is_err());
    }

    #[test]
    fn forge_events_round_trip_through_serde() {
        let event = ForgeEvent {
            id: "github:issue:7@2026-10-08T00:00:00Z".into(),
            kind: EventKind::Closed,
            repo: RepoRef::new("acme", "widgets"),
            number: 7,
            is_pr: false,
            actor: Some("alice".into()),
            body: None,
            url: None,
            timestamp: Timestamp::parse("2026-10-08T00:00:00Z").expect("parses"),
            raw: json!({ "number": 7 }),
        };
        let round: ForgeEvent =
            serde_json::from_value(serde_json::to_value(&event).expect("serializes"))
                .expect("deserializes");
        assert_eq!(round, event);
    }
}
