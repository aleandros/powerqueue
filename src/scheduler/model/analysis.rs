//! Advisory persistence interleaved with production launch/admission transitions.
use super::*;
use crate::analysis::{AnalysisInput, AnalysisRecord, Classification, Proposal, record_key};
use crate::strategies::AnalysisModelOp;

proptest! {
    #![proptest_config(model_config(32))]
    #[test]
    fn advisory_results_never_approve_retarget_or_reserve_resources(ops in analysis_model_ops()) {
        use AnalysisModelOp::*;
        let mut d = super::policy_reload::ready_world(1, 1);
        d.set_project("web", &(true, false, ModelTier::sonnet()));
        d.cfg.repo.path = "/repos/default".into();
        let mut project = crate::projects::ProjectConfig::default();
        project.repo.path = "/repos/web".into();
        d.cfg.projects.insert("web".into(), project);
        d.cfg.analysis.projects = [("default".into(), "API".into()), ("web".into(), "UI".into())].into();
        d.tasks[0].task.source = TaskSource::GitHub { repository: "example/issues".into(), number: 1, url: "https://github.com/example/issues/1".into() };
        d.tasks[0].task.github_admission = Some(crate::domain::GitHubAdmission::denied("human approval required"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analysis.db");
        let mut store = crate::store::Store::open(&path).unwrap();
        store.insert_task(&d.tasks[0].task).unwrap();
        let mut pending = None;
        let mut saved: Option<AnalysisRecord> = None;
        // Force advice before admission, then a real launch followed by a late result
        // and a rejected reassignment. Generated walks explore further reorderings.
        let prefix = [Observe(Some(true)), Finish, Launch, Approve(true), Observe(Some(true)), Launch, Finish, Reassign, Edit, Restart];
        for (step, op) in prefix.into_iter().chain(ops).enumerate() {
            d.now += Duration::seconds(1);
            store.update_task(&d.tasks[0].task).unwrap();
            match op {
                Observe(_) => {
                    let input = AnalysisInput::new(&d.cfg, &d.tasks[0].task).unwrap();
                    pending = Some(AnalysisRecord { created_at: d.now, fingerprint: input.fingerprint(&d.cfg),
                        proposal: Some(Proposal { project: Some("web".into()), classification: Classification::Feature,
                            confidence: 0.99, reasoning: "UI feature; requires human review".into(), plan: vec!["Inspect the UI".into()] }.validate(&input, 0.8).unwrap()), attempts: vec![] });
                }
                Finish => if let Some(record) = &pending {
                    let before = store.get_task(d.tasks[0].task.id).unwrap().unwrap();
                    let ledgers = d.ledgers.clone();
                    let slots = d.live_count();
                    let accepted = store.save_analysis(before.id, record).unwrap();
                    let expected = saved.as_ref().is_none_or(|s| s.created_at <= record.created_at);
                    prop_assert_eq!(accepted, expected);
                    if accepted { saved = Some(record.clone()); }
                    prop_assert_eq!(store.get_task(before.id).unwrap().unwrap(), before.clone());
                    prop_assert_eq!(d.ledgers.clone(), ledgers);
                    prop_assert_eq!(d.live_count(), slots);
                    prop_assert_eq!(&d.tasks[0].task.execution_project, &before.execution_project);
                },
                Edit => d.tasks[0].task.description.push_str(" changed"),
                Describe => { d.cfg.analysis.projects.get_mut("web").unwrap().push_str(" updated"); }
                Approve(approved) => {
                    d.step(step, 0, &Op::Admission(crate::domain::GitHubAdmission { approved, reason: "human action".into() }))?;
                }
                Launch => {
                    let approved = d.tasks[0].task.execution_approved();
                    let pass = d.launch_pass(step, &[])?;
                    if !approved { prop_assert_eq!(pass.starts, 0, "analysis bypassed source approval"); }
                    if step == 5 { prop_assert_eq!(pass.starts, 1, "forced prefix must launch"); }
                }
                Complete => { d.step(step, 0, &Op::Complete(None))?; }
                Retry => { d.step(step, 0, &Op::Retry)?; }
                Reassign => {
                    let before = d.tasks[0].task.clone();
                    d.step(step, 0, &Op::SetProject("web".into()))?;
                    if before.attempts > 0 || d.tasks[0].session.is_some() {
                        prop_assert_eq!(d.tasks[0].task.execution_project.clone(), before.execution_project, "started work retargeted");
                    }
                }
                Restart => { drop(store); store = crate::store::Store::open(&path).unwrap(); }
            }
            let id = d.tasks[0].task.id;
            prop_assert_eq!(store.kv_get::<AnalysisRecord>(&record_key(id)).unwrap(), saved.clone());
            if let Some(record) = &saved {
                let input = AnalysisInput::new(&d.cfg, &d.tasks[0].task).unwrap();
                prop_assert_eq!(record.stale(&d.cfg, &d.tasks[0].task), record.fingerprint != input.fingerprint(&d.cfg));
            }
        }
    }
}

proptest! {
    #![proptest_config(model_config(32))]
    #[test]
    fn automatic_routing_respects_human_choices_approval_and_execution_history(ops in analysis_model_ops()) {
        use AnalysisModelOp::*;
        for launch_after_prefix in [false, true] {
            let mut d = super::policy_reload::ready_world(1, 1);
            d.set_project("web", &(true, false, ModelTier::sonnet()));
            d.cfg.repo.path = "/repos/default".into();
            let mut project = crate::projects::ProjectConfig::default();
            project.repo.path = "/repos/web".into();
            d.cfg.projects.insert("web".into(), project);
            d.cfg.analysis.enabled = true;
            d.cfg.analysis.projects = [("default".into(), "API".into()), ("web".into(), "UI".into())].into();
            d.cfg.routing.mode = crate::routing::RoutingMode::Automatic;
            d.cfg.github.enabled = true;
            d.cfg.github.repository = "example/issues".into();
            d.tasks[0].task.source = TaskSource::GitHub { repository: "example/issues".into(), number: 1, url: String::new() };
            d.tasks[0].task.github_admission = Some(crate::domain::GitHubAdmission::denied("human approval required"));
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("automatic.db");
            let mut store = crate::store::Store::open(&path).unwrap();
            let id = d.tasks[0].task.id;
            store.insert_task(&d.tasks[0].task).unwrap();
            let mut pending = None;
            let mut prefix = vec![Launch, Observe(Some(true)), Edit, Finish, Observe(Some(true)), Finish,
                Observe(None), Finish, Launch, Observe(Some(false)), Finish, Observe(Some(true)), Finish, Restart];
            let forced_launch = if launch_after_prefix {
                prefix.extend([Approve(true), Launch]);
                Some(prefix.len() - 1)
            } else { None };
            // Exercise both a guaranteed successful launch after replacement and
            // generated operations while work is still unstarted.
            for (step, op) in prefix.into_iter().chain(ops.clone()).enumerate() {
                d.now += Duration::seconds(1);
                match op {
                    Observe(destination) => pending = Some(AnalysisRecord { created_at: d.now,
                        fingerprint: AnalysisInput::new(&d.cfg, &d.tasks[0].task).unwrap().fingerprint(&d.cfg),
                        proposal: Some(Proposal { project: destination.map(|web| if web { "web" } else { "default" }.into()), classification: Classification::Feature,
                            confidence: 0.95, reasoning: "UI ownership".into(), plan: vec!["Inspect UI".into()] }), attempts: vec![] }),
                    Finish => if let Some(record) = &pending {
                        let before = d.tasks[0].task.clone();
                        let slots = d.live_count();
                        let ledgers = d.ledgers.clone();
                        let mut eligible = before.clone();
                        if matches!(eligible.project_assignment, Some(crate::routing::ProjectAssignment::Automatic { .. })) {
                            eligible.project_assignment = None;
                        }
                        let can_replace = d.cfg.needs_project_analysis(&eligible)
                            && !record.stale(&d.cfg, &before)
                            && store.latest_session(id).unwrap().is_none()
                            && matches!(before.state, TaskState::Queued | TaskState::Paused | TaskState::Blocked | TaskState::Throttled | TaskState::NeedsAttention);
                        let (saved, applied) = store.save_and_apply_analysis(id, record, &d.cfg).unwrap();
                        let after = store.get_task(id).unwrap().unwrap();
                        if saved && can_replace {
                            let expected = record.proposal.as_ref().unwrap().project.as_deref();
                            prop_assert_eq!(after.project_assignment.is_some(), expected.is_some(), "replacement result must reconcile assignment");
                            if let Some(project) = expected { prop_assert_eq!(after.execution_project.as_str(), project); }
                            prop_assert_eq!(d.cfg.route(&after).permitted, expected.is_some());
                        }
                        if applied {
                            prop_assert!(!crate::routing::has_execution_history(&before));
                            prop_assert!(d.tasks[0].session.is_none());
                            prop_assert!(!matches!(before.project_assignment, Some(crate::routing::ProjectAssignment::Explicit { .. })), "human choice must win");
                            prop_assert!(!record.stale(&d.cfg, &before));
                            prop_assert_eq!(after.execution_project.as_str(), record.proposal.as_ref().unwrap().project.as_deref().unwrap_or(""));
                            prop_assert_eq!(d.cfg.route(&after).permitted, record.proposal.as_ref().unwrap().project.is_some());
                            prop_assert_eq!(&after.github_admission, &before.github_admission);
                            prop_assert_eq!(after.state, before.state);
                            prop_assert_eq!(&after.source, &before.source);
                        } else { prop_assert_eq!(&after, &before); }
                        prop_assert_eq!(d.live_count(), slots);
                        prop_assert_eq!(&d.ledgers, &ledgers);
                        d.tasks[0].task = after;
                    },
                    Edit => d.tasks[0].task.description.push_str(" edited"),
                    Describe => d.cfg.analysis.projects.get_mut("web").unwrap().push_str(" described"),
                    Approve(approved) => { d.step(step, 0, &Op::Admission(crate::domain::GitHubAdmission { approved, reason: "human action".into() }))?; }
                    Launch => {
                        let decision = d.cfg.route(&d.tasks[0].task);
                        transitions::on_routing(&mut d.tasks[0].task, decision, false);
                        let approved = d.tasks[0].task.execution_approved();
                        let pass = d.launch_pass(step, &[])?;
                        if !approved { prop_assert_eq!(pass.starts, 0); }
                        if forced_launch == Some(step) { prop_assert_eq!(pass.starts, 1, "replacement recovery plus approval must launch"); }
                    }
                    Complete => { d.step(step, 0, &Op::Complete(None))?; }
                    Retry => { d.step(step, 0, &Op::Retry)?; }
                    Reassign => {
                        let before = d.tasks[0].task.clone();
                        let result = store.set_execution_project(id, &before.execution_project, "default");
                        if crate::routing::has_execution_history(&before) || d.tasks[0].session.is_some() {
                            prop_assert!(result.is_err());
                        }
                        d.tasks[0].task = store.get_task(id).unwrap().unwrap();
                    }
                    Restart => { drop(store); store = crate::store::Store::open(&path).unwrap(); }
                }
                store.update_task(&d.tasks[0].task).unwrap();
                d.tasks[0].task = store.get_task(id).unwrap().unwrap();
                // The real store must see sessions too: finished sessions also prohibit retargeting.
                if let Some(session) = &d.tasks[0].session
                    && store.latest_session(id).unwrap().is_none() { store.insert_session(session).unwrap(); }
            }
        }
    }
}
