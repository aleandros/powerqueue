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
        let prefix = [Observe, Finish, Launch, Approve(true), Observe, Launch, Finish, Reassign, Edit, Restart];
        for (step, op) in prefix.into_iter().chain(ops).enumerate() {
            d.now += Duration::seconds(1);
            store.update_task(&d.tasks[0].task).unwrap();
            match op {
                Observe => {
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
