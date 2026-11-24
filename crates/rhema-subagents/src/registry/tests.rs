use super::*;

fn registry() -> Registry {
    Registry::new(2)
}

/// Arms `job` with placeholder spawn ingredients, the way a start
/// always does.
fn arm(registry: &Registry, id: &str) {
    registry.arm_restart(
        id,
        JobRerun {
            def: crate::def::SubagentDef {
                name: "scout".into(),
                description: "explores".into(),
                model: None,
                max_steps: None,
                body: "Explore.".into(),
            },
            prompt: "go".into(),
            parent_run: "turn-1".into(),
            model: None,
            hook: None,
            child: None,
        },
    );
}

#[test]
fn ids_are_minted_in_order_and_unique() {
    let registry = registry();
    assert_eq!(registry.mint("a").id, "job-1");
    assert_eq!(registry.mint("b").id, "job-2");
}

#[test]
fn a_fresh_job_reads_queued_and_an_unknown_id_is_unknown() {
    let registry = registry();
    let job = registry.mint("scout");
    assert_eq!(registry.status()[0].phase, "queued");
    assert_eq!(registry.status()[0].name, "scout");
    assert_eq!(
        registry.fetch(&job.id),
        Fetched::Busy {
            name: "scout".into()
        }
    );
    assert_eq!(registry.fetch("job-99"), Fetched::Unknown);
}

#[test]
fn a_fetched_ok_job_serves_its_cached_report_and_leaves_the_snapshot() {
    let registry = registry();
    let job = registry.mint("scout");
    registry.finish(&job.id, Ok("the report".to_owned()));
    // Finished: off the working snapshot at once, the finish event
    // pending, the announcement still steering.
    assert!(registry.status().is_empty());
    assert_eq!(
        registry.take_finished(),
        vec![JobFinished {
            id: "job-1".into(),
            name: "scout".into(),
            line: "Subagent scout (job-1) has finished.".into(),
        }]
    );
    assert_eq!(
        registry.notifications(),
        vec!["Subagent scout (job-1) has finished.".to_owned()]
    );
    let fetched = registry.fetch(&job.id);
    assert_eq!(
        fetched,
        Fetched::Report {
            name: "scout".into(),
            report: "the report".into()
        }
    );
    // The cache survives the fetch; the announcement does not.
    assert_eq!(
        registry.fetch(&job.id),
        Fetched::Report {
            name: "scout".into(),
            report: "the report".into()
        }
    );
    assert!(registry.status().is_empty());
    assert!(registry.notifications().is_empty());
}

#[test]
fn the_registry_keeps_at_most_the_cap_in_finished_reports() {
    let registry = registry();
    let first = registry.mint("scout");
    registry.finish(&first.id, Ok("the first report".to_owned()));
    let mut newest = first.id.clone();
    for _ in 1..(MAX_RETAINED_JOBS + 1) {
        newest = registry.mint("scout").id;
        registry.finish(&newest, Ok("the report".to_owned()));
    }

    assert_eq!(
        registry.fetch(&first.id),
        Fetched::Unknown,
        "the oldest report was evicted at the cap"
    );
    assert_eq!(
        registry.fetch(&newest),
        Fetched::Report {
            name: "scout".into(),
            report: "the report".into()
        },
        "the newest report survives"
    );
}

#[test]
fn an_unread_feed_drops_the_oldest_and_says_so_once() {
    let registry = registry();
    for _ in 0..(MAX_FINISHED + 8) {
        let job = registry.mint("scout");
        registry.finish(&job.id, Ok("the report".to_owned()));
    }
    let events = registry.take_finished();
    assert!(
        events.len() <= MAX_FINISHED + 1,
        "the cap holds (plus the overflow marker): {}",
        events.len()
    );
    assert!(
        events[0].id.is_empty() && events[0].line.contains("dropped"),
        "the overflow speaks first: {:?}",
        events[0].line
    );
    // The marker speaks once: the next take is clean.
    assert!(registry.take_finished().is_empty());
    let job = registry.mint("scout");
    registry.finish(&job.id, Ok("the report".to_owned()));
    let events = registry.take_finished();
    assert_eq!(events.len(), 1);
    assert!(
        events[0].id == job.id,
        "no stale marker: {:?}",
        events[0].line
    );
}

#[test]
fn the_finish_feed_delivers_each_job_once_in_mint_order() {
    let registry = registry();
    let ok = registry.mint("scout");
    let bad = registry.mint("reviewer");
    registry.finish(
        &bad.id,
        Err(JobFailure::Permanent(
            "hit its step budget of 256".to_owned(),
        )),
    );
    registry.finish(&ok.id, Ok("the report".to_owned()));
    assert_eq!(
            registry.take_finished(),
            vec![
                JobFinished {
                    id: "job-1".into(),
                    name: "scout".into(),
                    line: "Subagent scout (job-1) has finished.".into(),
                },
                JobFinished {
                    id: "job-2".into(),
                    name: "reviewer".into(),
                    line: "Subagent reviewer (job-2) has finished with an error: hit its step budget of 256."
                        .into(),
                },
            ],
            "oldest first, whatever the finish order"
        );
    assert!(registry.take_finished().is_empty(), "one event per job");
}

#[test]
fn a_taken_line_stops_announcing_but_a_finish_event_retires_nothing() {
    let registry = registry();
    let job = registry.mint("scout");
    registry.finish(&job.id, Ok("the report".to_owned()));
    // The notice feed alone leaves the steering line pending: the
    // driver still announces it.
    let _ = registry.take_finished();
    assert_eq!(
        registry.notifications(),
        vec!["Subagent scout (job-1) has finished.".to_owned()]
    );
    // The idle take retires the line: the prompt carries the text
    // now, and the driver's drain must not repeat it.
    assert_eq!(
        registry.take_notifications(),
        vec!["Subagent scout (job-1) has finished.".to_owned()]
    );
    assert!(registry.notifications().is_empty());
    assert!(registry.take_notifications().is_empty());
    // The report is still on the fetch.
    assert_eq!(
        registry.fetch(&job.id),
        Fetched::Report {
            name: "scout".into(),
            report: "the report".into()
        }
    );
}

#[test]
fn a_delivered_finish_queues_its_childs_settle() {
    let registry = registry();
    let job = registry.mint("scout");
    arm(&registry, &job.id);
    registry.announce_start(
        &job.id,
        Started {
            child: SessionId::new("child-1").expect("the id is valid"),
            def: "scout".into(),
            prompt: "go".into(),
            model: None,
        },
    );
    registry.finish(&job.id, Ok("the report".to_owned()));
    assert!(
        registry.take_settled().is_empty(),
        "a finish no one             delivered settles nothing: the load's re-announce is the \
            durable path for a crash here"
    );
    // The idle take delivers the line, and the delivery settles.
    assert_eq!(
        registry.take_notifications(),
        vec!["Subagent scout (job-1) has finished.".to_owned()]
    );
    assert_eq!(
        registry.take_settled(),
        vec![Settled {
            child: SessionId::new("child-1").expect("the id is valid"),
            outcome: Outcome::Done
        }]
    );
    assert!(registry.take_settled().is_empty(), "the settle is once");
    // A later fetch is the consumption of a delivered finish: no
    // second settle.
    assert_eq!(
        registry.fetch(&job.id),
        Fetched::Report {
            name: "scout".into(),
            report: "the report".into()
        }
    );
    assert!(registry.take_settled().is_empty());
}

#[test]
fn a_fetched_report_delivers_the_settle_itself() {
    let registry = registry();
    let job = registry.mint("scout");
    arm(&registry, &job.id);
    registry.announce_start(
        &job.id,
        Started {
            child: SessionId::new("child-2").expect("the id is valid"),
            def: "scout".into(),
            prompt: "go".into(),
            model: None,
        },
    );
    let failure = JobFailure::Transient("the link died".to_owned());
    registry.finish(&job.id, Err(failure));
    // No idle take ever lands; the model fetches directly.
    assert_eq!(
        registry.fetch(&job.id),
        Fetched::Failed {
            name: "scout".into(),
            reason: "the link died".into()
        }
    );
    assert_eq!(
        registry.take_settled(),
        vec![Settled {
            child: SessionId::new("child-2").expect("the id is valid"),
            outcome: Outcome::Failed {
                error: "the link died".to_owned(),
                retryable: true
            }
        }]
    );
    assert!(
        registry.take_notifications().is_empty(),
        "the fetch retired the line along with the settle"
    );
}

#[test]
fn a_job_without_a_child_queues_no_settle() {
    let registry = registry();
    let job = registry.mint("scout");
    registry.finish(&job.id, Ok("the report".to_owned()));
    assert_eq!(
        registry.take_notifications(),
        vec!["Subagent scout (job-1) has finished.".to_owned()]
    );
    // A start the registry never saw recorded no record, so the
    // delivery settles nothing.
    assert!(registry.take_settled().is_empty());
}

#[test]
fn the_idle_take_covers_only_finished_jobs() {
    let registry = registry();
    let done = registry.mint("alpha");
    let running = registry.mint("zeta");
    registry.finish(&done.id, Ok("r".to_owned()));
    registry.mark_running(&running.id, None);
    assert_eq!(
        registry.take_notifications(),
        vec!["Subagent alpha (job-1) has finished.".to_owned()],
        "a running job has no line to take"
    );
    // The running job keeps its snapshot line.
    let snapshot = registry.status();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].id, running.id);
    assert_eq!(snapshot[0].phase, "thinking");
}

#[test]
fn a_failed_job_fetches_as_its_reason() {
    let registry = registry();
    let job = registry.mint("reviewer");
    registry.finish(
        &job.id,
        Err(JobFailure::Permanent(
            "hit its step budget of 256".to_owned(),
        )),
    );
    // The failure announced itself before the fetch, notice
    // included.
    assert_eq!(
        registry.take_finished()[0].line,
        "Subagent reviewer (job-1) has finished with an error: hit its step budget of 256."
    );
    assert_eq!(
        registry.notifications(),
        vec![
            "Subagent reviewer (job-1) has finished with an error: hit its step budget of 256."
                .to_owned()
        ]
    );
    assert_eq!(
        registry.fetch(&job.id),
        Fetched::Failed {
            name: "reviewer".into(),
            reason: "hit its step budget of 256".into()
        }
    );
    assert!(
        registry.notifications().is_empty(),
        "fetched jobs stop announcing"
    );
}

#[test]
fn a_late_report_never_overwrites_a_finish() {
    let registry = registry();
    let job = registry.mint("scout");
    registry.finish(&job.id, Ok("final".to_owned()));
    registry.report(
        &job.id,
        &rhema_core::ToolStatus {
            phase: "running".to_owned(),
            context: None,
            spent: None,
            model: Some("m".into()),
        },
    );
    // The finish announced itself; the late status did not silence it.
    assert!(
        !registry.notifications().is_empty(),
        "the announcement survives a report it must ignore"
    );
    assert_eq!(
        registry.fetch(&job.id),
        Fetched::Report {
            name: "scout".into(),
            report: "final".into()
        },
        "a late status changes nothing: the finish stands"
    );
    assert!(
        registry.notifications().is_empty(),
        "and the fetch retires the announcement, like any fetched job"
    );
}

#[test]
fn a_running_job_reports_phase_context_and_spend() {
    let registry = registry();
    let job = registry.mint("scout");
    registry.mark_running(&job.id, Some("worker-model".into()));
    registry.report(
        &job.id,
        &rhema_core::ToolStatus {
            phase: "running".to_owned(),
            context: Some(rhema_core::Usage {
                input_tokens: 100,
                output_tokens: 10,
                cached_input_tokens: 90,
            }),
            spent: Some(rhema_core::Usage {
                input_tokens: 200,
                output_tokens: 20,
                cached_input_tokens: 0,
            }),
            model: None,
        },
    );
    let status = &registry.status()[0];
    assert_eq!(status.phase, "running");
    assert_eq!(status.model.as_deref(), Some("worker-model"));
    assert_eq!(status.context.map(|u| u.input_tokens), Some(100));
    assert_eq!(status.spent.map(|u| u.output_tokens), Some(20));
}

#[test]
fn status_and_notifications_come_out_oldest_first() {
    let registry = registry();
    let first = registry.mint("alpha");
    let second = registry.mint("zeta");
    let ids: Vec<String> = registry.status().iter().map(|s| s.id.clone()).collect();
    assert_eq!(ids, vec!["job-1", "job-2"], "mint order, not id order");
    registry.finish(&second.id, Ok("r2".to_owned()));
    registry.finish(&first.id, Ok("r1".to_owned()));
    assert_eq!(
        registry.notifications()[0],
        "Subagent alpha (job-1) has finished."
    );
    let events = registry.take_finished();
    let names: Vec<&str> = events.iter().map(|event| event.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "zeta"], "mint order, not id order");
    assert!(registry.status().is_empty(), "finished jobs leave it");
}

#[test]
fn dropping_the_registry_flips_every_cancel_flag() {
    let registry = registry();
    let job = registry.mint("scout");
    let cancel = job.cancel.clone();
    drop(registry);
    assert!(cancel.load(Ordering::Relaxed), "the job stops");
}

#[test]
fn a_transient_failure_restarts_in_place_and_retires_the_announcements() {
    let registry = registry();
    let job = registry.mint("scout");
    arm(&registry, &job.id);
    registry.finish(
        &job.id,
        Err(JobFailure::Transient("network error: dead link".to_owned())),
    );
    assert_eq!(
        registry.notifications(),
        vec![
            "Subagent scout (job-1) has finished with an error: network error: dead link."
                .to_owned()
        ]
    );

    let restarted = registry.restart_failed();
    assert_eq!(restarted.len(), 1);
    assert_eq!(restarted[0].id, job.id);
    assert_eq!(restarted[0].name, "scout");
    assert_eq!(restarted[0].rerun.prompt, "go");

    // Same id, back to queued, clock fresh, cancel flag fresh: the
    // re-run is the job's own continuation.
    let snapshot = registry.status();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].id, job.id);
    assert_eq!(snapshot[0].phase, "queued");
    assert_eq!(
        registry.fetch(&job.id),
        Fetched::Busy {
            name: "scout".into()
        },
        "the failed report's cache cleared with the state it described"
    );
    assert!(
        registry.notifications().is_empty(),
        "the stale failure never steers: the model would delegate beside the re-run"
    );
    assert!(
        !restarted[0].cancel.load(Ordering::Relaxed),
        "the re-run's flag mints fresh"
    );
}

#[test]
fn a_restarted_job_can_fail_transiently_and_restart_again() {
    let registry = registry();
    let job = registry.mint("scout");
    arm(&registry, &job.id);
    registry.finish(&job.id, Err(JobFailure::Transient("network".to_owned())));
    assert_eq!(registry.restart_failed().len(), 1);
    // The re-run failed the same way: the ingredients survive the
    // restart, so the retry command keeps working.
    registry.finish(&job.id, Err(JobFailure::Transient("network".to_owned())));
    assert_eq!(registry.restart_failed().len(), 1);
}

#[test]
fn only_transient_failures_restart() {
    let registry = registry();
    let transient = registry.mint("scout");
    arm(&registry, &transient.id);
    let permanent = registry.mint("reviewer");
    arm(&registry, &permanent.id);
    let finished = registry.mint("writer");
    arm(&registry, &finished.id);
    registry.finish(
        &transient.id,
        Err(JobFailure::Transient("network".to_owned())),
    );
    registry.finish(
        &permanent.id,
        Err(JobFailure::Permanent(
            "hit its step budget of 256".to_owned(),
        )),
    );
    registry.finish(&finished.id, Ok("the report".to_owned()));

    let restarted = registry.restart_failed();
    assert_eq!(
        restarted
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>(),
        vec![transient.id],
        "a spent budget and a finished report never re-run"
    );
}

#[test]
fn a_job_never_armed_never_restarts() {
    // Unreachable through the tool (every start arms), but the
    // fail-safe is no restart, never a spawn without ingredients.
    let registry = registry();
    let job = registry.mint("scout");
    registry.finish(&job.id, Err(JobFailure::Transient("network".to_owned())));
    assert!(registry.restart_failed().is_empty());
}

#[test]
fn restarts_come_out_in_mint_order() {
    let registry = registry();
    let first = registry.mint("alpha");
    let second = registry.mint("zeta");
    arm(&registry, &first.id);
    arm(&registry, &second.id);
    registry.finish(&second.id, Err(JobFailure::Transient("network".to_owned())));
    registry.finish(&first.id, Err(JobFailure::Transient("network".to_owned())));
    let restarted = registry.restart_failed();
    let ids: Vec<&str> = restarted.iter().map(|job| job.id.as_str()).collect();
    assert_eq!(ids, vec![first.id, second.id]);
}

#[tokio::test]
async fn a_queued_job_never_starts_after_the_registry_drops() {
    // Cap 1, slot taken: the queued wait parks. Dropping the
    // registry ends the wait with None instead of a permit.
    let registry = Arc::new(Registry::new(1));
    let slot = registry.try_slot().expect("the one slot");
    registry.mint("holder");
    let weak = Arc::downgrade(&registry);
    let waiter = tokio::spawn(async move { acquire_slot(&weak).await });
    tokio::time::sleep(SLOT_POLL * 3).await;
    drop(registry);
    drop(slot);
    assert!(waiter.await.unwrap().is_none(), "no permit past the drop");
}
