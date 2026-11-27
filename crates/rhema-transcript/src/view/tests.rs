use super::*;
use rhema_core::{Content, ToolCall, ToolResult};
use std::collections::HashSet;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// The test runtime is current-thread: spawned tasks run while the
/// test yields. Wait until `done` holds, then stop yielding.
async fn wait_for(done: impl Fn() -> bool) {
    for _ in 0..10_000 {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the background fill never finished");
}

/// A big tool result, in kilobytes.
fn result_bytes(kb: usize) -> String {
    "x".repeat(kb * 1024)
}

fn call_round(id: &str, result_kb: usize) -> Vec<Message> {
    vec![
        Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: vec![
                Content::Text("checking".into()),
                Content::ToolCall(ToolCall {
                    id: id.into(),
                    name: "bash".into(),
                    args: serde_json::json!({ "command": "make test" }),
                    provider_data: serde_json::Value::Null,
                }),
            ],
        },
        Message {
            annotations: Default::default(),
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult::success(
                id,
                result_bytes(result_kb),
            ))],
        },
    ]
}

/// A history over the default budget: many 8 KiB tool rounds, then
/// a final user message.
fn long_history(rounds: usize) -> Vec<Message> {
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    for i in 0..rounds {
        messages.extend(call_round(&format!("c{i}"), 8));
    }
    messages.push(Message::text(Role::User, "and now summarize"));
    messages
}

fn settings(max_view_bytes: usize) -> TranscriptSettings {
    TranscriptSettings {
        max_view_bytes,
        ..TranscriptSettings::default()
    }
}

#[test]
fn below_the_budget_the_view_is_none() {
    let mut transcript = Transcript::new(settings(128_000));
    let messages = long_history(4); // ~33 KiB
    assert!(transcript.view(&messages).is_none());
    let stats = transcript.stats();
    assert_eq!(stats.requests, 1);
    assert_eq!(stats.pass_through, 1);
    assert_eq!(stats.viewed, 0);
}

#[test]
fn past_the_budget_the_aged_prefix_is_block_and_the_tail_whole() {
    let mut transcript = Transcript::new(settings(128_000));
    let messages = long_history(30); // ~245 KiB
    let view = transcript.view(&messages).unwrap();

    // One synthetic user message first, tagged, carrying the
    // block's sections.
    assert_eq!(view[0].role, Role::User);
    let rhema_core::Content::Text(text) = &view[0].content[0] else {
        panic!("text view message");
    };
    assert!(text.starts_with("<transcript>"), "{text}");
    assert!(text.contains("zoom(id, n)"), "{text}");
    // Sections are addressed `id+n|`, tiling from the log's start.
    assert!(text.contains("0+"), "{text}");

    // The tail, the last user message, is present whole.
    let last = view.last().unwrap();
    assert_eq!(*last, Message::text(Role::User, "and now summarize"));
    assert!(view.len() < messages.len(), "the view is shorter");

    let stats = transcript.stats();
    assert_eq!(stats.flushes, 1);
    assert_eq!(stats.sectioned_messages, transcript.cut);
    assert!(stats.block_bytes > 0);
    assert!(stats.band_bytes + stats.block_bytes < 245 * 1024);
    // The token figures describe the same parts as the byte ones.
    assert!(stats.block_tokens > 0);
    assert!(stats.band_tokens > 0);
    assert!(stats.tail_tokens <= stats.band_tokens);
}

#[test]
fn between_flushes_the_block_is_append_only() {
    let mut transcript = Transcript::new(settings(40_000));
    let first = long_history(5); // ~40 KiB, over budget
    let view = transcript.view(&first).unwrap();
    let first_block = match &view[0].content[..] {
        [rhema_core::Content::Text(text)] => text.clone(),
        _ => panic!("text view message"),
    };
    let sections_after_first = transcript.sections.clone();

    // Growth well under the hysteresis: no new flush, the
    // committed sections byte-identical, only the band grows.
    let mut second = first.clone();
    second.extend(call_round("c5", 1));
    second.push(Message::text(Role::User, "next"));
    let view = transcript.view(&second).unwrap();
    let second_block = match &view[0].content[..] {
        [rhema_core::Content::Text(text)] => text.clone(),
        _ => panic!("text view message"),
    };
    assert_eq!(transcript.stats.flushes, 1, "no second flush");
    assert_eq!(transcript.sections, sections_after_first);
    assert_eq!(first_block, second_block, "the block is byte-identical");
    assert!(view.len() > 1);
}

#[test]
fn a_flush_overshoots_to_half_the_allowance_and_the_hysteresis_holds() {
    let mut transcript = Transcript::new(settings(128_000));
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    for i in 0..20 {
        messages.extend(call_round(&format!("c{i}"), 8)); // ~164 KiB
    }
    messages.push(Message::text(Role::User, "go on"));
    assert!(transcript.view(&messages).is_some());
    let stats = transcript.stats();
    assert_eq!(stats.flushes, 1);
    // The band the flush leaves behind sits at or below half the
    // allowance, the overshoot, not at the full allowance.
    let allowance = 128_000 - stats.tail_bytes;
    let band = stats.band_bytes - stats.tail_bytes;
    assert!(
        band <= allowance / 2,
        "band {band} over half the allowance {}",
        allowance / 2
    );
    // The hysteresis is the half-allowance the overshoot bought:
    // growth inside it must not flush again.
    let mut grown = messages.clone();
    for i in 20..26 {
        grown.extend(call_round(&format!("c{i}"), 8)); // ~49 KiB < allowance/2
    }
    grown.push(Message::text(Role::User, "still going"));
    transcript.view(&grown);
    assert_eq!(transcript.stats.flushes, 1, "flushed inside the hysteresis");
}

#[test]
fn a_flush_never_splits_a_tool_round_at_a_section_boundary() {
    let mut transcript = Transcript::new(settings(20_000));
    // user, round c0 (16 KiB), round c1 (16 KiB), user.
    let messages = vec![Message::text(Role::User, "go")]
        .into_iter()
        .chain(call_round("c0", 16))
        .chain(call_round("c1", 16))
        .chain(std::iter::once(Message::text(Role::User, "done")))
        .collect::<Vec<_>>();
    let view = transcript.view(&messages).unwrap();
    // Every tool result carried whole has its call carried too:
    // the pairs survive the cut together.
    let carried_calls: HashSet<String> = view
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            rhema_core::Content::ToolCall(call) => Some(call.id.clone()),
            _ => None,
        })
        .collect();
    for message in &view {
        for content in &message.content {
            if let rhema_core::Content::ToolResult(result) = content {
                assert!(
                    carried_calls.contains(&result.id),
                    "orphan result {} in the view",
                    result.id
                );
            }
        }
    }
    // And no section's range opens on a result: the same pairing
    // holds inside the block, so a section never stands in for a
    // round it splits.
    for section in &transcript.sections {
        let first = transcript
            .range(section.range)
            .expect("section in the log")
            .first()
            .unwrap();
        assert_ne!(first.role, Role::Tool, "section {:?}", section.range);
    }
}

#[test]
fn sections_respect_the_step_and_message_bounds() {
    let mut transcript = Transcript::new(TranscriptSettings {
        step_bytes: 20_000,
        max_section_messages: 3,
        ..settings(20_000)
    });
    let messages = long_history(8);
    transcript.observe(&messages);
    transcript.plan_flush();
    assert!(transcript.sections.len() > 1, "bounds force a split");
    for section in &transcript.sections {
        assert!(section.range.n <= 4, "{:?}", section.range);
    }
    // The sections tile the cut without gaps.
    let mut at = 0;
    for section in &transcript.sections {
        assert_eq!(section.range.id, at);
        at += section.range.n;
    }
    assert_eq!(at, transcript.cut);
}

#[test]
fn a_worker_upgrade_lands_and_a_wrong_one_is_rejected() {
    let mut transcript = Transcript::new(settings(20_000));
    let messages = long_history(30);
    transcript.observe(&messages);
    let pending = transcript.plan_flush();
    assert!(!pending.is_empty());

    let good = pending[0].clone();
    assert!(transcript.commit_summary(&good, "## Goals Met\n- all".into()));
    assert_eq!(transcript.sections[0].text, "## Goals Met\n- all");
    assert_eq!(transcript.sections[0].origin, SectionOrigin::Model);

    // A stale pending, the range moved on, is dropped.
    // The context is irrelevant to the commit's range check.
    let stale = PendingSummary {
        section: pending[0].section,
        range: Range { id: 999, n: 1 },
        messages: Vec::new(),
        context: None,
    };
    assert!(!transcript.commit_summary(&stale, "ghost".into()));
    assert_eq!(transcript.sections[0].text, "## Goals Met\n- all");
}

#[test]
fn a_resume_adopts_the_saved_sections_and_skips_the_refill() {
    let log = long_history(30);
    // The first session sections and upgrades the block.
    let mut first = Transcript::new(settings(20_000));
    first.observe(&log);
    let pending = first.plan_flush();
    assert!(!pending.is_empty());
    for item in &pending {
        assert!(first.commit_summary(item, format!("## Goals Met\n- model {}", item.section)));
    }
    let saved = first.saved_sections();
    assert!(saved.iter().all(|s| s.origin == SectionOrigin::Model));

    // The resumed session adopts them: the compaction the last
    // session paid for rides as-is, dense from the first request.
    let mut second = Transcript::new(settings(20_000));
    second.adopt_sections(saved.clone());
    second.observe(&log);
    assert_eq!(second.cut, first.cut);
    assert_eq!(second.sections, saved);
    // No re-sectioning, no pending fill: nothing to re-pay.
    assert!(second.plan_flush().is_empty());

    // The band keeps growing from the adopted cut.
    let mut extended = log.clone();
    extended.push(Message::text(Role::User, "one more"));
    second.observe(&extended);
    let view = second.render().unwrap();
    assert_eq!(view.last(), Some(&Message::text(Role::User, "one more")));
}

// --- the skill pin ---

/// A skill body, frontmatter included, at a known size.
fn skill_body(name: &str, rules: usize) -> String {
    let mut body = format!("---\nname: {name}\ndescription: the {name} trigger\n---\n\n");
    for i in 0..rules {
        body.push_str(&format!("Rule {i}: do the {name} thing.\n"));
    }
    body
}

/// The shell's wrapped form of a `cat` result: exit code, stdout,
/// stderr (shell.rs renders exactly this around a command's
/// output).
fn cat_result(id: &str, stdout: &str) -> Message {
    Message {
        annotations: Default::default(),
        role: Role::Tool,
        content: vec![Content::ToolResult(ToolResult::success(
            id,
            format!("exit code 0\n--- stdout ---\n{stdout}\n--- stderr ---\n"),
        ))],
    }
}

/// One turn reading a skill: the bash cat, then the wrapped body.
fn skill_round(id: &str, name: &str, rules: usize) -> Vec<Message> {
    let body = skill_body(name, rules);
    vec![
        Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: vec![
                Content::Text("loading the skill".into()),
                Content::ToolCall(ToolCall {
                    id: id.into(),
                    name: "bash".into(),
                    args: serde_json::json!({
                        "command": format!("cat .rhema/skills/{name}/SKILL.md")
                    }),
                    provider_data: serde_json::Value::Null,
                }),
            ],
        },
        cat_result(id, &body),
    ]
}

#[test]
fn a_flushed_skill_read_rides_the_block_whole_and_unsummarized() {
    let mut transcript = Transcript::new(settings(40_000));
    transcript.set_skills(vec![SkillText {
        name: "unslop".into(),
        body: skill_body("unslop", 40),
    }]);
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    messages.extend(skill_round("s1", "unslop", 40));
    messages.extend(long_history(6));
    messages.push(Message::text(Role::User, "and now summarize"));

    transcript.observe(&messages);
    let pending = transcript.plan_flush();
    let body = skill_body("unslop", 40);

    // The pinned span is its own two-message section, never queued
    // for the worker: no fill can rewrite the body into a count
    // line.
    let pinned = transcript
        .sections
        .iter()
        .find(|section| !section.pinned.is_empty())
        .expect("one pinned section");
    assert_eq!(pinned.pinned, ["unslop"]);
    assert_eq!(pinned.range.n, 2);
    assert!(pinned.text.contains(&body), "{}", pinned.text);
    assert!(!pending.iter().any(|summary| summary.range == pinned.range));

    // The rendered block carries the body verbatim, shell wrapper
    // and all detection passing.
    let view = transcript.render().unwrap();
    let rhema_core::Content::Text(text) = &view[0].content[0] else {
        panic!("text view message");
    };
    assert!(text.contains(&body), "{text}");
}

#[test]
fn a_reread_compacts_to_a_pointer_at_the_first_copy() {
    let mut transcript = Transcript::new(settings(40_000));
    transcript.set_skills(vec![SkillText {
        name: "unslop".into(),
        body: skill_body("unslop", 40),
    }]);
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    messages.extend(skill_round("s1", "unslop", 40));
    messages.extend(skill_round("s2", "unslop", 40));
    messages.extend(long_history(6));
    messages.push(Message::text(Role::User, "and now summarize"));

    transcript.observe(&messages);
    transcript.plan_flush();

    let pinned: Vec<_> = transcript
        .sections
        .iter()
        .filter(|section| !section.pinned.is_empty())
        .collect();
    assert_eq!(pinned.len(), 1, "one carrier");
    let carried = pinned[0].range;
    let body = skill_body("unslop", 40);
    let view = transcript.render().unwrap();
    let rhema_core::Content::Text(text) = &view[0].content[0] else {
        panic!("text view message");
    };
    assert_eq!(text.matches(&body).count(), 1, "one copy of the body");
    assert!(
        text.contains(&format!(
            "skill unslop read again; its whole text is carried at {}+{}|",
            carried.id, carried.n
        )),
        "{text}"
    );
}

#[test]
fn a_multicall_turn_digests_normally_without_a_pin() {
    let mut transcript = Transcript::new(settings(40_000));
    transcript.set_skills(vec![SkillText {
        name: "unslop".into(),
        body: skill_body("unslop", 40),
    }]);
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    // One turn, two calls: the skill cat plus an unrelated command.
    // Pinning would orphan the second result behind its call.
    messages.push(Message {
        annotations: Default::default(),
        role: Role::Assistant,
        content: vec![
            Content::ToolCall(ToolCall {
                id: "a".into(),
                name: "bash".into(),
                args: serde_json::json!({ "command": "cat the skill" }),
                provider_data: serde_json::Value::Null,
            }),
            Content::ToolCall(ToolCall {
                id: "b".into(),
                name: "bash".into(),
                args: serde_json::json!({ "command": "make test" }),
                provider_data: serde_json::Value::Null,
            }),
        ],
    });
    messages.push(cat_result("a", &skill_body("unslop", 40)));
    messages.push(cat_result("b", "ok"));
    messages.extend(long_history(6));
    messages.push(Message::text(Role::User, "and now summarize"));

    transcript.observe(&messages);
    transcript.plan_flush();
    assert!(
        transcript.sections.iter().all(|s| s.pinned.is_empty()),
        "{:?}",
        transcript.sections
    );
}

#[test]
fn consolidation_keeps_the_pinned_body_in_the_block() {
    let mut transcript = Transcript::new(TranscriptSettings {
        max_view_bytes: 40_000,
        step_bytes: 4_000,
        max_block_bytes: 1_000,
        ..TranscriptSettings::default()
    });
    transcript.set_skills(vec![SkillText {
        name: "unslop".into(),
        body: skill_body("unslop", 40),
    }]);
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    messages.extend(skill_round("s1", "unslop", 40));
    messages.extend(long_history(30));
    messages.push(Message::text(Role::User, "and now summarize"));

    transcript.observe(&messages);
    transcript.plan_flush();
    let body = skill_body("unslop", 40);
    assert!(
        transcript.stats.consolidations > 0,
        "the fixture must consolidate"
    );
    let view = transcript.render().unwrap();
    let rhema_core::Content::Text(text) = &view[0].content[0] else {
        panic!("text view message");
    };
    assert!(text.contains(&body), "{text}");
}

#[test]
fn the_model_fill_leaves_the_pinned_body_beneath_it() {
    let mut transcript = Transcript::new(TranscriptSettings {
        max_view_bytes: 40_000,
        step_bytes: 4_000,
        max_block_bytes: 1_000,
        ..TranscriptSettings::default()
    });
    transcript.set_skills(vec![SkillText {
        name: "unslop".into(),
        body: skill_body("unslop", 40),
    }]);
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    messages.extend(skill_round("s1", "unslop", 40));
    messages.extend(long_history(30));
    messages.push(Message::text(Role::User, "and now summarize"));

    transcript.observe(&messages);
    let pending = transcript.plan_flush();
    let body = skill_body("unslop", 40);

    // The consolidation's live union pending: its range still
    // matches the section, and the section carries the pin.
    let summary = pending
        .iter()
        .rev()
        .find(|summary| {
            transcript
                .sections
                .get(summary.section)
                .is_some_and(|section| section.range == summary.range && !section.pinned.is_empty())
        })
        .expect("a live union pending over a pinned section")
        .clone();
    assert!(transcript.commit_summary(&summary, "## Goals Met\n- dense fill".into()));
    let text = &transcript.sections[summary.section].text;
    assert!(text.starts_with("## Goals Met\n- dense fill"), "{text}");
    assert!(text.contains(&body), "{text}");
}

#[test]
fn a_fill_lands_after_the_flushs_own_consolidation_moves_it() {
    // One plan_flush both extends the block and consolidates it, so
    // the extend halves' pending indices are stamped before the
    // merges shift every section below them. Every fill whose
    // range a section still holds must land, and the fixture must
    // move at least one commissioned section off the index its
    // pending names.
    let mut transcript = Transcript::new(TranscriptSettings {
        max_view_bytes: 40_000,
        step_bytes: 4_000,
        max_block_bytes: 1_000,
        ..TranscriptSettings::default()
    });
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    messages.extend(long_history(30));
    messages.push(Message::text(Role::User, "and now summarize"));
    transcript.observe(&messages);
    let pending = transcript.plan_flush();
    assert!(
        transcript.stats.consolidations > 0,
        "the fixture must consolidate"
    );

    let moved = pending.iter().filter(|summary| {
        transcript
            .sections
            .get(summary.section)
            .is_none_or(|section| section.range != summary.range)
    });
    assert!(
        moved.count() > 0,
        "the fixture must move a commissioned section"
    );

    for summary in &pending {
        let index = transcript
            .sections
            .iter()
            .position(|section| section.range == summary.range);
        let committed = transcript.commit_summary(summary, "## dense fill".into());
        assert_eq!(
            index.is_some(),
            committed,
            "the range decides, not the index"
        );
        if let Some(index) = index {
            assert_eq!(transcript.sections[index].text, "## dense fill");
            assert!(matches!(
                transcript.sections[index].origin,
                SectionOrigin::Model
            ));
        }
    }
}

#[test]
fn a_merge_drops_a_pin_the_discovery_no_longer_holds() {
    // The pin map tells a re-read where a skill's body rides whole.
    // A merge that digests the body away while the discovery no
    // longer holds the skill must drop the claim too, or the map
    // points at a section that carries no body.
    let mut transcript = Transcript::new(TranscriptSettings {
        max_view_bytes: 40_000,
        step_bytes: 4_000,
        max_block_bytes: 1_000,
        ..TranscriptSettings::default()
    });
    transcript.set_skills(vec![SkillText {
        name: "unslop".into(),
        body: skill_body("unslop", 40),
    }]);
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    messages.extend(skill_round("s1", "unslop", 40));
    messages.extend(long_history(6));
    messages.push(Message::text(Role::User, "and now summarize"));
    transcript.observe(&messages);
    let pending = transcript.plan_flush();
    drop(pending);
    assert!(
        transcript
            .sections
            .iter()
            .any(|section| section.pinned.iter().any(|name| name == "unslop")),
        "the fixture must pin the skill first"
    );

    // The skill leaves the discovery, the session grows past the
    // block cap again, and the consolidating merge has nothing to
    // re-append.
    transcript.set_skills(Vec::new());
    let mut messages2 = messages;
    messages2.extend(long_history(6));
    messages2.push(Message::text(Role::User, "carry on"));
    transcript.observe(&messages2);
    let pending = transcript.plan_flush();
    drop(pending);
    assert!(
        transcript.stats.consolidations > 0,
        "the fixture must consolidate the merge"
    );
    assert!(
        transcript
            .sections
            .iter()
            .all(|section| !section.pinned.iter().any(|name| name == "unslop")),
        "a claim without its body is a lie"
    );
    // The map rebuilds from the committed claims, so the next
    // flush's pointers cannot name the bodyless union.
    transcript.observe(&messages2);
    assert!(!transcript.pinned.contains_key("unslop"));
}

#[test]
fn a_resume_keeps_deduplicating_rereads() {
    let mut first = Transcript::new(settings(40_000));
    first.set_skills(vec![SkillText {
        name: "unslop".into(),
        body: skill_body("unslop", 40),
    }]);
    let mut messages = vec![Message::text(Role::User, "fix the build")];
    messages.extend(skill_round("s1", "unslop", 40));
    messages.extend(long_history(8));
    messages.push(Message::text(Role::User, "and now summarize"));
    first.observe(&messages);
    first.plan_flush();
    let saved = first.saved_sections();
    assert!(!saved.is_empty(), "the first session must have flushed");

    // The resume adopts those sections, then a re-read of the same
    // skill flushes to a pointer, not a second body.
    let mut second = Transcript::new(settings(40_000));
    second.adopt_sections(saved);
    let mut resumed = messages.clone();
    resumed.extend(skill_round("s2", "unslop", 40));
    resumed.extend(long_history(2));
    resumed.push(Message::text(Role::User, "one more"));
    second.observe(&resumed);
    second.plan_flush();
    let body = skill_body("unslop", 40);
    let view = second.render().unwrap();
    let rhema_core::Content::Text(text) = &view[0].content[0] else {
        panic!("text view message");
    };
    assert_eq!(text.matches(&body).count(), 1, "one copy after resume");
}

#[test]
fn tampered_saved_sections_fall_back_to_the_rebuild() {
    let mut transcript = Transcript::new(settings(20_000));
    transcript.adopt_sections(vec![Section {
        range: Range { id: 5, n: 9 },
        text: "## Goals Met\n- orphan".into(),
        origin: SectionOrigin::Model,
        pinned: Vec::new(),
    }]);
    transcript.observe(&long_history(30));
    // The first id 5, not 0: the tiling rejects the adoption.
    assert_eq!(transcript.cut, 0);
    assert!(transcript.sections.is_empty());
    // The next request rebuilds from nothing.
    assert!(!transcript.plan_flush().is_empty());
}

#[test]
fn a_pending_summary_carries_the_adjacent_digest_as_context() {
    let mut transcript = Transcript::new(settings(20_000));
    transcript.observe(&long_history(30));
    let pending = transcript.plan_flush();
    assert!(pending.len() >= 2);
    // The first section has no predecessor; every later one points
    // at the stand-in committed for the section before it.
    assert_eq!(pending[0].context, None);
    for (prev, item) in pending.iter().zip(pending.iter().skip(1)) {
        let expected = transcript.sections[prev.section].text.clone();
        assert_eq!(item.context.as_deref(), Some(expected.as_str()));
    }

    // A later flush's first chunk inherits the block's last
    // section as its context.
    let first = pending[0].clone();
    assert!(transcript.commit_summary(&first, "## Goals Met\n- model".into()));
    let mut extended = long_history(30);
    for i in 0..12 {
        extended.push(Message::text(
            Role::User,
            format!("extra filler {i}: {}", "y".repeat(1024)),
        ));
    }
    transcript.observe(&extended);
    // Read the predecessor before planning: plan_flush itself
    // pushes the new sections.
    let expected = transcript.sections.last().unwrap().text.clone();
    let pending = transcript.plan_flush();
    assert!(!pending.is_empty());
    assert_eq!(pending[0].context.as_deref(), Some(expected.as_str()));
}

#[test]
fn the_block_consolidates_past_its_cap() {
    let mut transcript = Transcript::new(TranscriptSettings {
        max_block_bytes: 500,
        ..settings(20_000)
    });
    let messages = long_history(30);
    transcript.observe(&messages);
    transcript.plan_flush();
    assert!(transcript.block_bytes() <= 500);
    assert!(transcript.stats.consolidations > 0);
    // The surviving sections still tile the prefix.
    let mut at = 0;
    for section in &transcript.sections {
        assert_eq!(section.range.id, at);
        at += section.range.n;
    }
    assert_eq!(at, transcript.cut);
}

#[test]
fn a_resumed_session_rebuilds_the_compaction_from_nothing() {
    let mut transcript = Transcript::new(settings(20_000));
    transcript.observe(&long_history(30));
    transcript.plan_flush();
    assert!(transcript.cut > 0);
    // A different history wholesale: the derived state resets.
    transcript.observe(&long_history(2));
    assert_eq!(transcript.cut, 0);
    assert!(transcript.sections.is_empty());
}

#[test]
fn the_breakdown_spreads_reported_input_by_wire_share() {
    let mut transcript = Transcript::new(TranscriptSettings::default());
    transcript.set_skills_bytes(500);
    // System: 1500 base bytes + 500 skills bytes. Messages: one
    // user text, one assistant text with a call, one result.
    let system = format!("{}{}", "b".repeat(1_500), "s".repeat(500));
    let call_args = serde_json::json!({ "c": "x".repeat(100) });
    let messages = vec![
        Message::text(Role::User, "u".repeat(1_000)),
        Message {
            annotations: Default::default(),
            role: Role::Assistant,
            content: vec![
                Content::Text("a".repeat(400)),
                Content::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: call_args.clone(),
                    provider_data: serde_json::Value::Null,
                }),
            ],
        },
        Message {
            annotations: Default::default(),
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult::success(
                "c1",
                "r".repeat(1_000),
            ))],
        },
    ];
    transcript.stage_usage_shares(Some(&system), &[], &messages);
    transcript.record_usage(&Usage {
        cost_usd: None,
        input_tokens: 10_000,
        output_tokens: 100,
        cached_input_tokens: 0,
    });

    let total = 2_000 + 1_000 + 400 + ("bash".len() + call_args.to_string().len()) + 1_000;
    let spread = |bytes: usize| 10_000 * bytes as u64 / total as u64;
    let b = transcript.stats().usage;
    assert_eq!(b.system_prompt_tokens, spread(1_500));
    assert_eq!(b.skills_tokens, spread(500));
    assert_eq!(
        b.tool_calls_input_tokens,
        spread("bash".len() + call_args.to_string().len())
    );
    assert_eq!(b.tool_calls_output_tokens, spread(1_000));
    assert_eq!(b.user_messages_tokens, spread(1_000));
    // The model's words: the history's assistant text plus the
    // generated output.
    assert_eq!(b.model_output_tokens, spread(400) + 100);
    // The shares consumed: a usage record without a staged request
    // spreads nothing.
    transcript.record_usage(&Usage {
        cost_usd: None,
        input_tokens: 1_000,
        output_tokens: 0,
        cached_input_tokens: 0,
    });
    assert_eq!(transcript.stats().usage.user_messages_tokens, spread(1_000));
}

#[test]
fn a_skills_share_past_the_system_clamps() {
    let mut transcript = Transcript::new(TranscriptSettings::default());
    transcript.set_skills_bytes(9_999);
    transcript.stage_usage_shares(Some("short"), &[], &[]);
    transcript.record_usage(&Usage {
        cost_usd: None,
        input_tokens: 100,
        output_tokens: 0,
        cached_input_tokens: 0,
    });
    let b = transcript.stats().usage;
    assert_eq!(b.skills_tokens, 100);
    assert_eq!(b.system_prompt_tokens, 0);
}

#[test]
fn a_carry_continues_the_saved_run() {
    let saved = TranscriptStats {
        requests: 10,
        pass_through: 6,
        viewed: 4,
        flushes: 1,
        consolidations: 1,
        summarizer_calls: 2,
        summarizer_failures: 1,
        input_tokens: 90_000,
        cached_input_tokens: 70_000,
        output_tokens: 10_000,
        usage: UsageBreakdown {
            system_prompt_tokens: 20_000,
            skills_tokens: 2_000,
            tool_calls_input_tokens: 30_000,
            tool_calls_output_tokens: 30_000,
            user_messages_tokens: 8_000,
            model_output_tokens: 10_000,
        },
        // The saved snapshot describes the log the resume replaced.
        log_messages: 12,
        sectioned_messages: 4,
        sections: 2,
        block_bytes: 900,
        block_tokens: 225,
        band_bytes: 3_000,
        band_tokens: 820,
        view_images: 2,
        tail_bytes: 1_200,
        tail_tokens: 340,
        log_bytes: 5_400,
        log_content: LogContent {
            tool_calls: 100,
            tool_results: 4_000,
            user_text: 300,
            assistant_text: 1_000,
        },
    };

    // A mirror that has not looked at a log yet shows the saved
    // run whole, the counters and, until its first request, the
    // snapshot too.
    let mut fresh = TranscriptStats::default();
    fresh.carry(&saved);
    assert_eq!(fresh, saved);

    // The mirror then counts requests of its own: each read folds
    // the same saved run in once, so the totals are the run's,
    // never doubled, and the snapshot is the live mirror's.
    let mut live = TranscriptStats {
        requests: 2,
        input_tokens: 5_000,
        output_tokens: 300,
        usage: UsageBreakdown {
            user_messages_tokens: 5_000,
            model_output_tokens: 300,
            ..UsageBreakdown::default()
        },
        log_messages: 4,
        block_bytes: 100,
        ..TranscriptStats::default()
    };
    live.carry(&saved);
    assert_eq!(live.requests, 12);
    assert_eq!(live.input_tokens, 95_000);
    assert_eq!(live.output_tokens, 10_300);
    assert_eq!(live.usage.user_messages_tokens, 13_000);
    assert_eq!(live.usage.model_output_tokens, 10_300);
    assert_eq!(live.usage.system_prompt_tokens, 20_000);
    assert_eq!(live.block_bytes, 100, "the live mirror's snapshot");
    assert_eq!(live.log_messages, 4);

    // A later read: the mirror grew, the baseline still folds in
    // once.
    let mut later = TranscriptStats {
        requests: 3,
        input_tokens: 8_000,
        ..TranscriptStats::default()
    };
    later.carry(&saved);
    assert_eq!(later.requests, 13);
    assert_eq!(later.input_tokens, 98_000);
}

/// One user message per turn, each carrying one bare image: the
/// image-cap fixtures. Bytes stay tiny, so the byte budget never
/// binds and the cap alone drives any flush.
fn image_history(n: usize) -> Vec<Message> {
    (0..n)
        .map(|i| Message {
            annotations: Default::default(),
            role: Role::User,
            content: vec![
                Content::Text(format!("look {i}")),
                Content::Image(rhema_core::Image::new(
                    rhema_core::ImageMediaType::Png,
                    "AAAA",
                )),
            ],
        })
        .collect()
}

/// The cap flushes below the byte budget: five one-image turns at
/// a cap of two section the oldest three, the band keeps its two
/// newest images, and the digest covers the prefix.
#[test]
fn the_image_cap_flushes_below_the_byte_budget() {
    let mut transcript = Transcript::new(TranscriptSettings {
        max_view_images: 2,
        ..settings(128_000)
    });
    let messages = image_history(5);
    transcript.observe(&messages);
    let pending = transcript.plan_flush();
    assert!(
        !pending.is_empty(),
        "the cap flushes what bytes never would"
    );
    assert_eq!(transcript.cut, 3);
    assert_eq!(transcript.stats.flushes, 1);
    // The sections tile exactly the prefix the cut moved over.
    let mut at = 0;
    for section in &transcript.sections {
        assert_eq!(section.range.id, at);
        at += section.range.n;
    }
    assert_eq!(at, 3);
    // The digest names the images it covers: zoom has a reason.
    let digest = &transcript.sections.last().unwrap().text;
    assert!(digest.contains("3 images"), "{digest}");
    transcript.render();
    assert_eq!(transcript.stats().view_images, 2);
}

/// The tail exception: one turn carrying more images than the cap
/// sends whole, and the stat shows the overflow instead of a cut
/// into the turn the model is working from.
#[test]
fn one_turn_past_the_image_cap_sends_whole_and_counts() {
    let mut transcript = Transcript::new(TranscriptSettings {
        max_view_images: 2,
        ..settings(128_000)
    });
    let messages = vec![Message {
        annotations: Default::default(),
        role: Role::User,
        content: vec![
            Content::Text("three shots".into()),
            Content::Image(rhema_core::Image::new(rhema_core::ImageMediaType::Png, "A")),
            Content::Image(rhema_core::Image::new(rhema_core::ImageMediaType::Png, "B")),
            Content::Image(rhema_core::Image::new(rhema_core::ImageMediaType::Png, "C")),
        ],
    }];
    transcript.observe(&messages);
    assert!(transcript.plan_flush().is_empty());
    assert_eq!(transcript.cut, 0);
    transcript.render();
    assert_eq!(transcript.stats().view_images, 3);
}

/// The log-size snapshot: what the conversation holds, each
/// message counted once, refreshed on every request, pass-through
/// ones included. The usage split's once-counted counterpart.
#[test]
fn the_log_bytes_snapshot_counts_the_log_once() {
    let mut transcript = Transcript::new(settings(128_000));
    let messages = long_history(4); // ~33 KiB, below the budget
    transcript.observe(&messages);
    transcript.render();
    let expected: usize = messages.iter().map(summary::message_bytes).sum();
    assert_eq!(transcript.stats().log_bytes, expected);

    // An extension rides in; the next request recounts the log.
    let grown = long_history(5);
    transcript.observe(&grown);
    transcript.render();
    let expected: usize = grown.iter().map(summary::message_bytes).sum();
    assert_eq!(transcript.stats().log_bytes, expected);
}

/// The content split: the log's estimated tokens by category, each
/// message counted once. For this fixture the categories tile the
/// log, no images, no system or tool-role text, so they sum to
/// the log's whole estimate.
#[test]
fn the_content_split_counts_each_message_once() {
    let mut transcript = Transcript::new(settings(128_000));
    let messages = long_history(4);
    transcript.observe(&messages);
    transcript.render();
    let content = transcript.stats().log_content;
    assert_eq!(
        content.tool_results,
        4 * summary::text_tokens(result_bytes(8).len())
    );
    assert_eq!(
        content.assistant_text,
        4 * summary::text_tokens("checking".len())
    );
    assert_eq!(
        content.user_text,
        summary::text_tokens("fix the build".len())
            + summary::text_tokens("and now summarize".len())
    );
    let args = serde_json::json!({ "command": "make test" }).to_string();
    assert_eq!(
        content.tool_calls,
        4 * summary::text_tokens("bash".len() + args.len())
    );
    // The categories tile the log's tokens: the figure the Usage
    // page heads the split with.
    let expected: u64 = messages.iter().map(summary::message_tokens).sum();
    assert_eq!(content.total(), expected);
}

#[test]
fn an_image_costs_one_message_in_the_split_and_the_budget() {
    let mut transcript = Transcript::new(settings(128_000));
    transcript.observe(&[Message {
        annotations: Default::default(),
        role: Role::Tool,
        content: vec![Content::ToolResult(
            ToolResult::success("c1", "ok").with_images(vec![rhema_core::Image::new(
                rhema_core::ImageMediaType::Png,
                "A".repeat(2_000_000),
            )]),
        )],
    }]);
    transcript.render();
    let stats = transcript.stats();
    // Two megabytes of base64 is one message: "ok" plus the flat
    // charge, so the split still tiles the log's bytes.
    assert_eq!(stats.log_bytes, 2 + summary::IMAGE_BYTES);
    // In tokens the image is its vision price, not a byte count.
    assert_eq!(stats.log_content.tool_results, summary::IMAGE_TOKENS);
}

#[test]
fn reset_stats_zeroes_the_accounting_and_keeps_the_log() {
    let mut transcript = Transcript::new(TranscriptSettings::default());
    transcript.observe(&[Message::text(Role::User, "kept")]);
    transcript.record_usage(&Usage {
        cost_usd: None,
        input_tokens: 1_000,
        output_tokens: 100,
        cached_input_tokens: 0,
    });
    assert_eq!(transcript.len(), 1);
    transcript.reset_stats();
    assert_eq!(transcript.stats(), TranscriptStats::default());
    assert_eq!(transcript.len(), 1);
}

#[test]
fn usage_records_and_the_hit_rate_reads() {
    let mut transcript = Transcript::new(TranscriptSettings::default());
    assert!(transcript.stats().cache_hit_rate().is_none());
    transcript.record_usage(&Usage {
        cost_usd: None,
        input_tokens: 1_000,
        output_tokens: 100,
        cached_input_tokens: 800,
    });
    let stats = transcript.stats();
    assert_eq!(stats.input_tokens, 1_000);
    assert_eq!(stats.cached_input_tokens, 800);
    let rate = stats.cache_hit_rate().unwrap();
    assert!((rate - 0.8).abs() < 1e-9);
}

/// Records what it was handed.
struct Recording(StdMutex<Vec<Vec<Message>>>);
#[async_trait]
impl ModelProvider for Recording {
    async fn complete(
        &self,
        req: rhema_core::ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        self.0.lock().unwrap().push(req.messages);
        Ok(ModelOutput {
            message: Message::text(Role::Assistant, "done"),
            stop_reason: rhema_core::StopReason::EndTurn,
            usage: Usage {
                cost_usd: None,
                input_tokens: 500,
                output_tokens: 10,
                cached_input_tokens: 400,
            },
        })
    }
}

#[tokio::test]
async fn the_decorator_rewrites_the_request_and_the_mirror_stays_whole() {
    let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
    let transcript = Arc::new(Mutex::new(Transcript::new(settings(20_000))));
    let view = TranscriptView::new(recording.clone(), transcript.clone());
    let messages = long_history(30);
    let req = rhema_core::ModelRequest {
        system: None,
        messages: messages.clone(),
        tools: Vec::new(),
        params: rhema_core::ChatParams::new("m"),
    };
    let mut sink = NoopSink;
    view.complete(req, &mut sink).await.unwrap();

    let seen = recording.0.lock().unwrap();
    let sent = &seen[0];
    assert!(sent.len() < messages.len());
    assert_eq!(sent.last().unwrap().role, Role::User);
    // The mirror kept every message, none dropped.
    let mirror = transcript.lock().unwrap();
    assert_eq!(mirror.len(), messages.len());
    assert_eq!(mirror.message(0), Some(&messages[0]));
    // The usage landed: the hit rate is reportable.
    assert_eq!(mirror.stats().cache_hit_rate(), Some(0.8));
}

#[tokio::test]
async fn the_decorator_upgrades_sections_with_its_summarizer() {
    /// Answers every call with the same dense block.
    struct Worker;
    #[async_trait]
    impl ModelProvider for Worker {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            Ok(ModelOutput {
                message: Message::text(Role::Assistant, "## Goals Met\n- worker wrote this"),
                stop_reason: rhema_core::StopReason::EndTurn,
                usage: Usage::default(),
            })
        }
    }

    let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
    let transcript = Arc::new(Mutex::new(Transcript::new(settings(20_000))));
    let view = TranscriptView::with_summarizer(
        recording.clone(),
        transcript.clone(),
        Arc::new(compact::ModelSummarizer::new(Arc::new(Worker), "cheap")),
    );
    let req = rhema_core::ModelRequest {
        system: None,
        messages: long_history(30),
        tools: Vec::new(),
        params: rhema_core::ChatParams::new("m"),
    };
    let mut sink = NoopSink;
    view.complete(req, &mut sink).await.unwrap();

    // The request went out with the extractive stand-ins: the fill
    // had not landed when it rendered.
    {
        let sent = recording.0.lock().unwrap();
        let rhema_core::Content::Text(text) = &sent[0][0].content[0] else {
            panic!("text view message");
        };
        assert!(!text.contains("worker wrote this"), "{text}");
    }

    // The background fill upgrades every section.
    wait_for(|| {
        transcript
            .lock()
            .unwrap()
            .sections
            .iter()
            .all(|s| s.origin == SectionOrigin::Model)
    })
    .await;

    // The guard drops at the brace: the runtime is current-thread,
    // so the next request cannot take the lock while it is held.
    {
        let mirror = transcript.lock().unwrap();
        let stats = mirror.stats();
        assert!(stats.summarizer_calls > 0);
        assert_eq!(stats.summarizer_failures, 0);
        assert!(
            mirror
                .sections
                .iter()
                .all(|s| s.text.contains("worker wrote this"))
        );
    }

    // The next request carries the dense text.
    let req = rhema_core::ModelRequest {
        system: None,
        messages: long_history(30),
        tools: Vec::new(),
        params: rhema_core::ChatParams::new("m"),
    };
    let mut sink = NoopSink;
    view.complete(req, &mut sink).await.unwrap();
    let sent = recording.0.lock().unwrap();
    let rhema_core::Content::Text(text) = &sent[1][0].content[0] else {
        panic!("text view message");
    };
    assert!(text.contains("worker wrote this"), "{text}");
}

#[tokio::test]
async fn a_failing_summarizer_never_blocks_the_request() {
    struct Failing;
    #[async_trait]
    impl ModelProvider for Failing {
        async fn complete(
            &self,
            _req: rhema_core::ModelRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ModelOutput, ProviderError> {
            Err(ProviderError::Timeout)
        }
    }

    let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
    let transcript = Arc::new(Mutex::new(Transcript::new(settings(20_000))));
    let view = TranscriptView::with_summarizer(
        recording.clone(),
        transcript.clone(),
        Arc::new(compact::ModelSummarizer::new(Arc::new(Failing), "cheap")),
    );
    let req = rhema_core::ModelRequest {
        system: None,
        messages: long_history(30),
        tools: Vec::new(),
        params: rhema_core::ChatParams::new("m"),
    };
    let mut sink = NoopSink;
    // The main call went out with the extractive stand-ins, and
    // never waited on the worker: the failure is counted by the
    // background fill, after the response.
    view.complete(req, &mut sink).await.unwrap();
    {
        // The guard drops before the wait: no lock is held across
        // an await.
        let sent = recording.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
    }
    wait_for(|| transcript.lock().unwrap().stats().summarizer_calls > 0).await;
    let mirror = transcript.lock().unwrap();
    assert_eq!(
        mirror.stats().summarizer_failures,
        mirror.stats().summarizer_calls
    );
    assert!(
        mirror
            .sections
            .iter()
            .all(|s| s.origin == SectionOrigin::Extractive)
    );
}

/// A summarizer worker that answers only once its gate opens; it
/// spins on yields, so an inline await of it would hang the
/// current-thread runtime.
struct Gated(Arc<AtomicBool>);

#[async_trait]
impl ModelProvider for Gated {
    async fn complete(
        &self,
        _req: rhema_core::ModelRequest,
        _sink: &mut dyn EventSink,
    ) -> Result<ModelOutput, ProviderError> {
        while !self.0.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        Ok(ModelOutput {
            message: Message::text(Role::Assistant, "## Goals Met\n- late but dense"),
            stop_reason: rhema_core::StopReason::EndTurn,
            usage: Usage::default(),
        })
    }
}

#[tokio::test]
async fn a_slow_summarizer_never_delays_the_request() {
    let gate = Arc::new(AtomicBool::new(false));
    let recording = Arc::new(Recording(StdMutex::new(Vec::new())));
    let transcript = Arc::new(Mutex::new(Transcript::new(settings(20_000))));
    let view = TranscriptView::with_summarizer(
        recording.clone(),
        transcript.clone(),
        Arc::new(compact::ModelSummarizer::new(
            Arc::new(Gated(Arc::clone(&gate))),
            "cheap",
        )),
    );
    let req = rhema_core::ModelRequest {
        system: None,
        messages: long_history(30),
        tools: Vec::new(),
        params: rhema_core::ChatParams::new("m"),
    };
    let turn = tokio::spawn(async move {
        let mut sink = NoopSink;
        view.complete(req, &mut sink).await.unwrap();
    });

    // The request goes out, and the turn finishes, while the worker
    // is still gated: the fill never sat in the request path.
    wait_for(|| !recording.0.lock().unwrap().is_empty()).await;
    wait_for(|| turn.is_finished()).await;
    assert!(
        !gate.load(Ordering::SeqCst),
        "the request waited on the gated worker"
    );

    // Then the fill lands, and the section upgrades.
    gate.store(true, Ordering::SeqCst);
    wait_for(|| {
        transcript
            .lock()
            .unwrap()
            .sections
            .iter()
            .all(|s| s.origin == SectionOrigin::Model)
    })
    .await;
    let mirror = transcript.lock().unwrap();
    assert!(
        mirror
            .sections
            .iter()
            .all(|s| s.text.contains("late but dense")),
        "{:?}",
        mirror.sections
    );
}

struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&mut self, _event: rhema_core::Event<'_>) {}
}
