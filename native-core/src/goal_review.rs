//! Privacy-bounded, read-only Goal Review projection. The caller must supply
//! a snapshot obtained from a validated GoalDriver. A snapshot alone cannot
//! independently prove persisted origin, provider execution or user consent.
//! This module does not read, mutate, persist, or run anything.
//!
//! Turn history and approval history lack a shared timestamp. The output puts
//! turns first (in turn order) and approvals second (in approval history order).
//! It MUST NOT be shown as one interleaved chronological event timeline.

use crate::mobile_goal::{GoalSnapshot, GoalStatus, TurnOutcome, TurnTicket};
use serde_json::{json, Value};
use std::collections::HashSet;
use uuid::Uuid;

const MAX_STEPS: usize = 16;
const MAX_TURNS: usize = 64;
const MAX_APPROVALS: usize = MAX_TURNS + MAX_STEPS;
const MAX_EVENTS: usize = MAX_TURNS + MAX_APPROVALS;
const MAX_JSON_BYTES: usize = 32 * 1024;

fn invalid() -> String {
    "Invalid Goal Review checkpoint; no evidence was produced".to_owned()
}

fn canonical_uuid(raw: &str) -> bool {
    raw.len() == 36 && Uuid::parse_str(raw).is_ok_and(|parsed| parsed.to_string() == raw)
}

fn status_name(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Ready => "ready",
        GoalStatus::AwaitingApproval => "awaiting_approval",
        GoalStatus::Paused => "paused",
        GoalStatus::InTurn => "in_turn",
        GoalStatus::Completed => "completed",
        GoalStatus::Failed => "failed",
        GoalStatus::Rejected => "rejected",
        GoalStatus::Cancelled => "cancelled",
    }
}

fn turn_kind(outcome: &TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Completed(_) => "user_attested_completed",
        TurnOutcome::RetryableFailure(_) => "retryable_failure",
        TurnOutcome::TerminalFailure(_) => "terminal_failure",
        TurnOutcome::NeedsApproval(_) => "needs_approval",
    }
}

fn validate_ticket(
    ticket: &TurnTicket,
    expected_step: usize,
    expected_attempt: u8,
    expected_turn: u16,
    step_count: usize,
    ids: &mut HashSet<String>,
) -> Result<(), String> {
    if !canonical_uuid(&ticket.id)
        || !ids.insert(ticket.id.clone())
        || ticket.step_index != expected_step
        || ticket.step_index >= step_count
        || ticket.attempt != expected_attempt
        || ticket.turn_number != expected_turn
    {
        return Err(invalid());
    }
    Ok(())
}

/// Fixed, bounded Goal Review receipt derived solely from checkpoint counters
/// and historical records. No details, notes, prompts, keys, tool results, or
/// step descriptions appear. The caller must not claim independently verified.
///
/// Top-level keys: goalId, revision, status, pausedFrom, currentStep,
/// totalSteps, turnsUsed, turnBudget, turnsRemaining, retriesUsedOnStep,
/// historyEventCount, approvalEventCount, independentlyVerified:false,
/// crossStreamChronologyKnown:false, eventOrder, events.
///
/// Each event: type, kind, stepIndex, attempt, turnNumber, ticketId,
/// independentlyVerified:false. Manual approval events have null values for
/// attempt, turnNumber and ticketId because ApprovalRecord lacks these fields.
pub fn summarize(snapshot: &GoalSnapshot) -> Result<Value, String> {
    let spec = &snapshot.spec;
    let step_count = spec.steps.len();
    let turns = snapshot.history.len();
    let approvals = snapshot.approvals.len();
    if snapshot.version != 1
        || snapshot.revision == u64::MAX
        || !canonical_uuid(&snapshot.id)
        || !(1..=MAX_STEPS).contains(&step_count)
        || (spec.max_turns as usize) > MAX_TURNS
        || (spec.max_turns as usize) < step_count
        || spec.max_retries_per_step > 5
        || snapshot.current_step > step_count
        || snapshot.turns_used > spec.max_turns
        || snapshot.retries_used_on_step > spec.max_retries_per_step.saturating_add(1)
        || turns > MAX_TURNS
        || approvals > MAX_APPROVALS
        || turns
            .checked_add(approvals)
            .is_none_or(|sum| sum > MAX_EVENTS)
    {
        return Err(invalid());
    }

    let effective_status = match (snapshot.status, snapshot.paused_from) {
        (GoalStatus::Paused, Some(GoalStatus::Ready)) => GoalStatus::Ready,
        (GoalStatus::Paused, Some(GoalStatus::AwaitingApproval)) => GoalStatus::AwaitingApproval,
        (GoalStatus::Paused, _) | (_, Some(_)) => return Err(invalid()),
        (status, None) => status,
    };
    if snapshot.status == GoalStatus::Completed {
        if snapshot.current_step != step_count {
            return Err(invalid());
        }
    } else if snapshot.current_step >= step_count {
        return Err(invalid());
    }
    if matches!(effective_status, GoalStatus::Ready | GoalStatus::InTurn)
        && (snapshot.approval_reason.is_some()
            || snapshot.stop_reason.is_some()
            || (spec.steps[snapshot.current_step].requires_approval && !snapshot.approved_for_step))
    {
        return Err(invalid());
    }
    if effective_status == GoalStatus::AwaitingApproval
        && (snapshot.approval_reason.is_none()
            || snapshot.approved_for_step
            || snapshot.stop_reason.is_some()
            || snapshot.turns_used >= spec.max_turns)
    {
        return Err(invalid());
    }
    if snapshot.status.is_terminal() {
        if (snapshot.status == GoalStatus::Completed && snapshot.stop_reason.is_some())
            || (snapshot.status != GoalStatus::Completed && snapshot.stop_reason.is_none())
            || snapshot.approval_reason.is_some()
        {
            return Err(invalid());
        }
    } else if snapshot.stop_reason.is_some() {
        return Err(invalid());
    }

    let mut events = Vec::with_capacity(turns + approvals);
    let mut seen = HashSet::with_capacity(turns + 1);
    let mut completed_steps = 0usize;
    let mut retries = 0u8;
    for (i, record) in snapshot.history.iter().enumerate() {
        validate_ticket(
            &record.ticket,
            completed_steps,
            retries.checked_add(1).ok_or_else(invalid)?,
            u16::try_from(i + 1).map_err(|_| invalid())?,
            step_count,
            &mut seen,
        )?;
        events.push(json!({
            "type": "turn",
            "kind": turn_kind(&record.outcome),
            "stepIndex": record.ticket.step_index,
            "attempt": record.ticket.attempt,
            "turnNumber": record.ticket.turn_number,
            "ticketId": record.ticket.id,
            "independentlyVerified": false
        }));
        match &record.outcome {
            TurnOutcome::Completed(_) => {
                completed_steps += 1;
                retries = 0;
            }
            TurnOutcome::RetryableFailure(_) => {
                retries = retries.checked_add(1).ok_or_else(invalid)?;
                if retries > spec.max_retries_per_step && i + 1 < turns {
                    return Err(invalid());
                }
            }
            TurnOutcome::TerminalFailure(_) => {
                if i + 1 != turns {
                    return Err(invalid());
                }
            }
            TurnOutcome::NeedsApproval(_) => {}
        }
    }
    if completed_steps != snapshot.current_step
        || retries != snapshot.retries_used_on_step
        || turns > snapshot.turns_used as usize
    {
        return Err(invalid());
    }
    let missing_one = turns + 1 == snapshot.turns_used as usize;
    match snapshot.status {
        GoalStatus::InTurn => {
            if !missing_one || snapshot.cancelled_turn.is_some() {
                return Err(invalid());
            }
            let active = snapshot.active_turn.as_ref().ok_or_else(invalid)?;
            validate_ticket(
                active,
                completed_steps,
                retries.checked_add(1).ok_or_else(invalid)?,
                snapshot.turns_used,
                step_count,
                &mut seen,
            )?;
        }
        GoalStatus::Cancelled => {
            if snapshot.active_turn.is_some()
                || (turns != snapshot.turns_used as usize && !missing_one)
            {
                return Err(invalid());
            }
            if let Some(cancelled) = &snapshot.cancelled_turn {
                if !missing_one {
                    return Err(invalid());
                }
                validate_ticket(
                    cancelled,
                    completed_steps,
                    retries.checked_add(1).ok_or_else(invalid)?,
                    snapshot.turns_used,
                    step_count,
                    &mut seen,
                )?;
            }
        }
        _ => {
            if snapshot.active_turn.is_some()
                || snapshot.cancelled_turn.is_some()
                || turns != snapshot.turns_used as usize
            {
                return Err(invalid());
            }
        }
    }
    if let Some(last) = snapshot.history.last() {
        match &last.outcome {
            TurnOutcome::TerminalFailure(reason) => {
                if snapshot.status != GoalStatus::Failed
                    || snapshot.stop_reason.as_deref() != Some(reason)
                {
                    return Err(invalid());
                }
            }
            TurnOutcome::RetryableFailure(reason)
                if retries > spec.max_retries_per_step
                    || (snapshot.turns_used == spec.max_turns && !missing_one) =>
            {
                if snapshot.status != GoalStatus::Failed
                    || snapshot.stop_reason.as_deref() != Some(reason)
                {
                    return Err(invalid());
                }
            }
            TurnOutcome::NeedsApproval(_) | TurnOutcome::Completed(_)
                if snapshot.turns_used == spec.max_turns
                    && !missing_one
                    && snapshot.current_step < step_count =>
            {
                if snapshot.status != GoalStatus::Failed
                    || snapshot.stop_reason.as_deref() != Some("Turn budget exhausted")
                {
                    return Err(invalid());
                }
            }
            _ => {
                if snapshot.status == GoalStatus::Failed {
                    return Err(invalid());
                }
            }
        }
        if matches!(last.outcome, TurnOutcome::NeedsApproval(_))
            && last.ticket.step_index == snapshot.current_step
            && matches!(effective_status, GoalStatus::Ready | GoalStatus::InTurn)
            && !snapshot.approved_for_step
        {
            return Err(invalid());
        }
    } else if snapshot.status == GoalStatus::Failed {
        return Err(invalid());
    }
    for (i, approval) in snapshot.approvals.iter().enumerate() {
        if approval.step_index > snapshot.current_step
            || approval.step_index >= step_count
            || (i > 0 && snapshot.approvals[i - 1].step_index > approval.step_index)
            || (!approval.approved && i + 1 != approvals)
        {
            return Err(invalid());
        }
        events.push(json!({
            "type": "manual_approval",
            "kind": if approval.approved { "approved" } else { "declined" },
            "stepIndex": approval.step_index,
            "attempt": null,
            "turnNumber": null,
            "ticketId": null,
            "independentlyVerified": false
        }));
    }
    if snapshot.status == GoalStatus::Rejected
        && !snapshot
            .approvals
            .last()
            .is_some_and(|record| !record.approved && record.step_index == snapshot.current_step)
    {
        return Err(invalid());
    }
    if snapshot
        .approvals
        .last()
        .is_some_and(|record| !record.approved)
        && snapshot.status != GoalStatus::Rejected
    {
        return Err(invalid());
    }
    if snapshot.approved_for_step
        && !snapshot
            .approvals
            .last()
            .is_some_and(|record| record.approved && record.step_index == snapshot.current_step)
    {
        return Err(invalid());
    }

    let review = json!({
        "goalId": snapshot.id,
        "revision": snapshot.revision,
        "status": status_name(snapshot.status),
        "pausedFrom": snapshot.paused_from.map(status_name),
        "currentStep": snapshot.current_step,
        "totalSteps": step_count,
        "turnsUsed": snapshot.turns_used,
        "turnBudget": spec.max_turns,
        "turnsRemaining": spec.max_turns - snapshot.turns_used,
        "retriesUsedOnStep": snapshot.retries_used_on_step,
        "historyEventCount": turns,
        "approvalEventCount": approvals,
        "independentlyVerified": false,
        "crossStreamChronologyKnown": false,
        "eventOrder": "turn_history_then_approval_history",
        "events": events,
    });
    if serde_json::to_vec(&review).map_err(|_| invalid())?.len() > MAX_JSON_BYTES {
        return Err(invalid());
    }
    Ok(review)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mobile_goal::{GoalDriver, GoalSpec, GoalStep, TurnOutcome};
    use std::{fs, path::PathBuf};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("synthetic-goal-review-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn spec(&self) -> GoalSpec {
            GoalSpec {
                title: "PRIVATE GOAL TITLE SECRET_TITLE_734".to_string(),
                steps: vec![
                    GoalStep {
                        description: "PRIVATE STEP SECRET_STEP_734".into(),
                        requires_approval: false,
                    },
                    GoalStep {
                        description: "PRIVATE APPROVAL SECRET_STEP_735".into(),
                        requires_approval: true,
                    },
                ],
                max_turns: 8,
                max_retries_per_step: 1,
            }
        }

        fn checkpoint(&self) -> Vec<u8> {
            fs::read(self.0.join("goal-v1.json")).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn only_fixed_event_fields(event: &Value) {
        let fields = event.as_object().unwrap();
        assert_eq!(fields.len(), 7);
        for key in fields.keys() {
            assert!(matches!(
                key.as_str(),
                "type"
                    | "kind"
                    | "stepIndex"
                    | "attempt"
                    | "turnNumber"
                    | "ticketId"
                    | "independentlyVerified"
            ));
        }
        assert_eq!(event["independentlyVerified"], false);
    }

    #[test]
    fn freshly_persisted_goal_projects_deterministically_without_writing() {
        let f = Fixture::new();
        let driver = GoalDriver::create(&f.0, f.spec()).unwrap();
        let live = driver.snapshot();
        let bytes = f.checkpoint();
        let first = summarize(&live).unwrap();
        let second = summarize(&live).unwrap();
        assert_eq!(first, second);
        assert_eq!(f.checkpoint(), bytes);
        assert_eq!(first["goalId"], live.id);
        assert_eq!(first["revision"], 0);
        assert_eq!(first["status"], "ready");
        assert!(first["pausedFrom"].is_null());
        assert_eq!(first["turnsUsed"], 0);
        assert_eq!(first["turnBudget"], 8);
        assert_eq!(first["turnsRemaining"], 8);
        assert_eq!(first["historyEventCount"], 0);
        assert_eq!(first["approvalEventCount"], 0);
        assert_eq!(first["events"], json!([]));
        assert_eq!(first["independentlyVerified"], false);
        assert_eq!(first["crossStreamChronologyKnown"], false);
        assert_eq!(first["eventOrder"], "turn_history_then_approval_history");
        drop(driver);
        let reopened = GoalDriver::open(&f.0).unwrap();
        assert_eq!(summarize(&reopened.snapshot()).unwrap(), first);
        assert_eq!(f.checkpoint(), bytes);
    }

    #[test]
    fn real_retry_manual_approval_and_completed_turns_never_leak_details() {
        let f = Fixture::new();
        let mut driver = GoalDriver::create(&f.0, f.spec()).unwrap();
        let retry = driver.begin_turn().unwrap();
        driver
            .record_outcome(
                &retry.id,
                TurnOutcome::RetryableFailure("PRIVATE FAILURE SECRET_RETRY_734".into()),
            )
            .unwrap();
        let dynamic = driver.begin_turn().unwrap();
        driver
            .record_outcome(
                &dynamic.id,
                TurnOutcome::NeedsApproval("PRIVATE REASON SECRET_REASON_734".into()),
            )
            .unwrap();
        driver
            .resolve_approval(true, "PRIVATE NOTE SECRET_NOTE_734")
            .unwrap();
        let completed = driver.begin_turn().unwrap();
        driver
            .record_outcome(
                &completed.id,
                TurnOutcome::Completed("PRIVATE MODEL SECRET_RESULT_734".into()),
            )
            .unwrap();
        assert_eq!(driver.snapshot().status, GoalStatus::AwaitingApproval);
        let waiting = driver.pause().unwrap();
        assert_eq!(waiting.paused_from, Some(GoalStatus::AwaitingApproval));
        let checkpoint_before = f.checkpoint();
        drop(driver);

        let mut reopened = GoalDriver::open(&f.0).unwrap();
        let report = summarize(&reopened.snapshot()).unwrap();
        assert_eq!(report["status"], "paused");
        assert_eq!(report["pausedFrom"], "awaiting_approval");
        assert_eq!(report["currentStep"], 1);
        assert_eq!(report["turnsUsed"], 3);
        assert_eq!(report["turnsRemaining"], 5);
        assert_eq!(report["historyEventCount"], 3);
        assert_eq!(report["approvalEventCount"], 1);
        assert_eq!(report["events"].as_array().unwrap().len(), 4);
        assert_eq!(report["events"][0]["kind"], "retryable_failure");
        assert_eq!(report["events"][1]["kind"], "needs_approval");
        assert_eq!(report["events"][2]["kind"], "user_attested_completed");
        assert_eq!(report["events"][0]["ticketId"], retry.id);
        assert_eq!(report["events"][1]["ticketId"], dynamic.id);
        assert_eq!(report["events"][2]["ticketId"], completed.id);
        assert_eq!(report["events"][0]["attempt"], 1);
        assert_eq!(report["events"][1]["attempt"], 2);
        assert_eq!(report["events"][2]["attempt"], 2);
        assert_eq!(report["events"][0]["turnNumber"], 1);
        assert_eq!(report["events"][2]["turnNumber"], 3);
        assert_eq!(report["events"][3]["type"], "manual_approval");
        assert_eq!(report["events"][3]["kind"], "approved");
        assert_eq!(report["events"][3]["stepIndex"], 0);
        assert!(report["events"][3]["attempt"].is_null());
        assert!(report["events"][3]["turnNumber"].is_null());
        assert!(report["events"][3]["ticketId"].is_null());
        for event in report["events"].as_array().unwrap() {
            only_fixed_event_fields(event);
        }
        assert_eq!(checkpoint_before, f.checkpoint());
        let as_text = serde_json::to_string(&report).unwrap();
        for secret in [
            "SECRET_TITLE_734",
            "SECRET_STEP_734",
            "SECRET_STEP_735",
            "SECRET_RETRY_734",
            "SECRET_REASON_734",
            "SECRET_NOTE_734",
            "SECRET_RESULT_734",
        ] {
            assert!(!as_text.contains(secret), "leaked: {secret}");
        }
        assert!(as_text.len() < MAX_JSON_BYTES);
        assert!(!as_text.contains("independentlyVerified\":true"));

        // Resume does not fabricate any previous outcome or approve a gate.
        reopened.resume().unwrap();
        assert!(reopened.begin_turn().is_err());
        reopened
            .resolve_approval(true, "PRIVATE NOTE SECRET_NOTE_735")
            .unwrap();
        let last = reopened.begin_turn().unwrap();
        reopened
            .record_outcome(
                &last.id,
                TurnOutcome::Completed("PRIVATE RESULT SECRET_FINAL_735".into()),
            )
            .unwrap();
        let final_view = summarize(&reopened.snapshot()).unwrap();
        assert_eq!(final_view["status"], "completed");
        assert_eq!(final_view["currentStep"], 2);
        assert_eq!(final_view["turnsUsed"], 4);
        assert_eq!(final_view["approvalEventCount"], 2);
        assert_eq!(final_view["historyEventCount"], 4);
        assert_eq!(final_view["events"][5]["kind"], "approved");
        assert!(!final_view.to_string().contains("SECRET_FINAL_735"));
    }

    #[test]
    fn declined_and_terminal_failed_checkpoints_report_only_record_types() {
        let denied = Fixture::new();
        let mut gated = denied.spec();
        gated.steps[0].requires_approval = true;
        let mut driver = GoalDriver::create(&denied.0, gated).unwrap();
        driver
            .resolve_approval(false, "PRIVATE SECRET_DENIED_734")
            .unwrap();
        drop(driver);
        let recovered = GoalDriver::open(&denied.0).unwrap();
        let denied_view = summarize(&recovered.snapshot()).unwrap();
        assert_eq!(denied_view["status"], "rejected");
        assert_eq!(denied_view["events"].as_array().unwrap().len(), 1);
        assert_eq!(denied_view["events"][0]["kind"], "declined");
        assert_eq!(denied_view["approvalEventCount"], 1);
        assert!(!denied_view.to_string().contains("SECRET_DENIED_734"));

        let failed = Fixture::new();
        let mut driver = GoalDriver::create(&failed.0, failed.spec()).unwrap();
        let turn = driver.begin_turn().unwrap();
        driver
            .record_outcome(
                &turn.id,
                TurnOutcome::TerminalFailure("PRIVATE SECRET_TERMINAL_734".into()),
            )
            .unwrap();
        drop(driver);
        let recovered = GoalDriver::open(&failed.0).unwrap();
        let failure_view = summarize(&recovered.snapshot()).unwrap();
        assert_eq!(failure_view["status"], "failed");
        assert_eq!(failure_view["historyEventCount"], 1);
        assert_eq!(failure_view["events"][0]["kind"], "terminal_failure");
        assert!(!failure_view.to_string().contains("SECRET_TERMINAL_734"));
    }

    #[test]
    fn active_or_cancelled_provider_turn_is_not_invented_as_completed_event() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let turn = goal.begin_turn().unwrap();
        let in_turn = summarize(&goal.snapshot()).unwrap();
        assert_eq!(in_turn["status"], "in_turn");
        assert_eq!(in_turn["turnsUsed"], 1);
        assert_eq!(in_turn["historyEventCount"], 0);
        assert_eq!(in_turn["events"], json!([]));
        assert!(!in_turn.to_string().contains(&turn.id));
        goal.cancel("PRIVATE SECRET_CANCELLED_734").unwrap();
        drop(goal);
        let recovered = GoalDriver::open(&f.0).unwrap();
        let cancelled = summarize(&recovered.snapshot()).unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["turnsUsed"], 1);
        assert_eq!(cancelled["historyEventCount"], 0);
        assert_eq!(cancelled["events"], json!([]));
        assert!(!cancelled.to_string().contains(&turn.id));
        assert!(!cancelled.to_string().contains("SECRET_CANCELLED_734"));
    }

    #[test]
    fn rejects_impossible_indices_replay_status_and_bounded_counts() {
        let f = Fixture::new();
        let mut goal = GoalDriver::create(&f.0, f.spec()).unwrap();
        let ticket = goal.begin_turn().unwrap();
        goal.record_outcome(&ticket.id, TurnOutcome::NeedsApproval("Synthetic".into()))
            .unwrap();
        goal.resolve_approval(true, "Synthetic user").unwrap();
        let valid = goal.snapshot();
        assert!(summarize(&valid).is_ok());
        let mut corruptions = Vec::new();
        let mut x = valid.clone();
        x.version = 2;
        corruptions.push(x);
        let mut x = valid.clone();
        x.revision = u64::MAX;
        corruptions.push(x);
        let mut x = valid.clone();
        x.id = "not-a-goal-id".into();
        corruptions.push(x);
        let mut x = valid.clone();
        x.current_step = 99;
        corruptions.push(x);
        let mut x = valid.clone();
        x.turns_used = 65;
        corruptions.push(x);
        let mut x = valid.clone();
        x.status = GoalStatus::Completed;
        corruptions.push(x);
        let mut x = valid.clone();
        x.status = GoalStatus::Paused;
        corruptions.push(x);
        let mut x = valid.clone();
        x.paused_from = Some(GoalStatus::InTurn);
        corruptions.push(x);
        let mut x = valid.clone();
        x.paused_from = Some(GoalStatus::AwaitingApproval);
        corruptions.push(x);
        let mut x = valid.clone();
        x.history[0].ticket.step_index = 1;
        corruptions.push(x);
        let mut x = valid.clone();
        x.history[0].ticket.turn_number = 9;
        corruptions.push(x);
        let mut x = valid.clone();
        x.history[0].ticket.attempt = 2;
        corruptions.push(x);
        let mut x = valid.clone();
        x.history[0].ticket.id = "fake".into();
        corruptions.push(x);
        let mut x = valid.clone();
        x.approvals[0].step_index = 1;
        corruptions.push(x);
        let mut x = valid.clone();
        x.approvals[0].approved = false;
        corruptions.push(x);
        let mut x = valid.clone();
        x.approvals = vec![valid.approvals[0].clone(); MAX_APPROVALS + 1];
        corruptions.push(x);
        let mut x = valid.clone();
        x.history = vec![valid.history[0].clone(); MAX_TURNS + 1];
        corruptions.push(x);
        for (index, fake) in corruptions.iter().enumerate() {
            assert!(summarize(fake).is_err(), "tampering variant {index}");
        }
        assert!(summarize(&valid).is_ok());
    }

    #[test]
    fn long_real_history_remains_within_fixed_json_limit_and_no_fabricated_events() {
        let f = Fixture::new();
        let spec = GoalSpec {
            title: "PRIVATE SECRET_LONG_HISTORY_734".into(),
            steps: vec![GoalStep {
                description: "PRIVATE SECRET_LONG_STEP_734".into(),
                requires_approval: false,
            }],
            max_turns: MAX_TURNS as u16,
            max_retries_per_step: 0,
        };
        let mut driver = GoalDriver::create(&f.0, spec).unwrap();
        for _ in 0..MAX_TURNS - 1 {
            let t = driver.begin_turn().unwrap();
            driver
                .record_outcome(
                    &t.id,
                    TurnOutcome::NeedsApproval("PRIVATE SECRET_LONG_REASON_734".into()),
                )
                .unwrap();
            driver
                .resolve_approval(true, "PRIVATE SECRET_LONG_NOTE_734")
                .unwrap();
        }
        let last = driver.begin_turn().unwrap();
        driver
            .record_outcome(
                &last.id,
                TurnOutcome::Completed("PRIVATE SECRET_LAST_734".into()),
            )
            .unwrap();
        drop(driver);
        let recovered = GoalDriver::open(&f.0).unwrap();
        let bytes = f.checkpoint();
        let review = summarize(&recovered.snapshot()).unwrap();
        assert_eq!(review["turnsUsed"], MAX_TURNS);
        assert_eq!(review["historyEventCount"], MAX_TURNS);
        assert_eq!(review["approvalEventCount"], MAX_TURNS - 1);
        assert_eq!(
            review["events"].as_array().unwrap().len(),
            MAX_TURNS * 2 - 1
        );
        let encoded = serde_json::to_vec(&review).unwrap();
        assert!(
            encoded.len() < MAX_JSON_BYTES,
            "output length {}",
            encoded.len()
        );
        assert!(!String::from_utf8(encoded)
            .unwrap()
            .contains("PRIVATE SECRET"));
        assert_eq!(bytes, f.checkpoint());
    }
}
