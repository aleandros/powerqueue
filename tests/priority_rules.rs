//! `priority.toml` parsing and evaluation through the public API, plus the
//! sync → store → evaluate round trip.

use chrono::{Duration, Utc};
use powerqueue::config::LinearConfig;
use powerqueue::domain::{Criticality, ModelTier, Task, TaskSource};
use powerqueue::linear::{LinearIssue, sync_issues};
use powerqueue::priority::rules::condition_matches;
use powerqueue::priority::{Condition, PriorityRules, template};
use powerqueue::store::Store;

fn rules() -> PriorityRules {
    PriorityRules::parse(template()).expect("template parses")
}

fn linear_task(key: &str, title: &str) -> Task {
    Task::new(
        key,
        title,
        TaskSource::Linear {
            issue_id: format!("uuid-{key}"),
            identifier: key.into(),
            url: "https://linear.app/x".into(),
            team_key: "ENG".into(),
        },
    )
}

#[test]
fn template_parses_cleanly() {
    let r = rules();
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert_eq!(r.default_criticality, Criticality::Normal);
    assert_eq!(r.models[&Criticality::Critical], vec![ModelTier::fable()]);
    assert_eq!(r.models[&Criticality::High], vec![ModelTier::opus()]);
    assert_eq!(r.model_for(Criticality::Normal), &[ModelTier::sonnet()]);
    assert!(!r.jev.enabled);
    assert_eq!(r.jev.levels.len(), 4);
}

#[test]
fn incident_label_is_critical_on_fable() {
    let r = rules();
    let mut t = linear_task("ENG-1", "Prod down");
    t.labels = vec!["Incident".into()];
    let e = r.evaluate(&t, t.created_at, None, 300.0, 2.0);
    assert_eq!(e.criticality, Criticality::Critical);
    assert_eq!(e.model, Some(ModelTier::fable()));
    assert_eq!(e.models, vec![ModelTier::fable()]);
    assert!(!e.skip);
    assert_eq!(e.score, Criticality::Critical.base_score());
    assert!(e.reasons.iter().any(|r| r.starts_with("critical: matched rule at line")), "{:?}", e.reasons);
}

#[test]
fn customer_high_priority_adds_scoring_bonuses() {
    let r = rules();
    let mut t = linear_task("ENG-2", "Customer asks for export");
    t.labels = vec!["customer".into()];
    t.linear_priority = Some(2);
    t.estimate = Some(13.0);
    let e = r.evaluate(&t, t.created_at, None, 300.0, 0.0);
    assert_eq!(e.criticality, Criticality::High);
    // base 500 + 40 (customer) + 20 (priority high) - 30 (estimate > 8)
    assert_eq!(e.score, 530.0);
    assert_eq!(e.model, Some(ModelTier::opus()));
}

#[test]
fn new_high_priority_bug_runs_before_older_high_priority_work() {
    let r = rules();
    let mut bug = linear_task("AVS-2089", "Requested date displays the previous day");
    bug.linear_priority = Some(2);
    bug.labels = vec!["Bug".into()];
    let mut ordinary = linear_task("AVS-2061", "Inventory search");
    ordinary.linear_priority = Some(2);
    let now = bug.created_at;
    for hours in [70, 100, 1000] {
        ordinary.created_at = now - Duration::hours(hours);
        for task in [&mut bug, &mut ordinary] {
            let evaluation = r.evaluate(task, now, None, 0.0, 2.0);
            task.criticality = evaluation.criticality;
            task.score = evaluation.score;
        }
        let tasks = [ordinary.clone(), bug.clone()];
        assert_eq!(powerqueue::scheduler::lifecycle::pick_next(&tasks, now).unwrap().key, bug.key);
    }
}

#[test]
fn manual_task_without_matches_is_normal_with_bonus_and_age() {
    let r = rules();
    let t = Task::new("manual-1", "Tidy up", TaskSource::Manual);
    let later = t.created_at + Duration::hours(3);
    let e = r.evaluate(&t, later, Some(0.5), 300.0, 2.0);
    assert_eq!(e.criticality, Criticality::Normal);
    // base 100 + 10 (source manual) + 150 (jev) + 6 (age)
    assert!((e.score - 266.0).abs() < 1e-6, "{}", e.score);
    assert_eq!(e.model, Some(ModelTier::sonnet()));
    assert!(e.reasons.iter().any(|x| x.starts_with("jev 0.50 × 300 = +150")), "{:?}", e.reasons);
    assert!(e.reasons.iter().any(|x| x.starts_with("age +6.0 (3.0h)")), "{:?}", e.reasons);
}

#[test]
fn chore_label_is_low() {
    let r = rules();
    let mut t = linear_task("ENG-3", "Bump deps");
    t.labels = vec!["chore".into()];
    t.linear_priority = Some(3);
    // `## Normal` (priority: normal) comes before `## Low`, so this is normal.
    assert_eq!(r.evaluate(&t, t.created_at, None, 0.0, 0.0).criticality, Criticality::Normal);
    t.linear_priority = Some(0);
    assert_eq!(r.evaluate(&t, t.created_at, None, 0.0, 0.0).criticality, Criticality::Low);
}

#[test]
fn overrides_pin_tickets() {
    let md = format!(
        "{}\n[[overrides]]\nkey = \"ENG-9\"\nskip = true\n[[overrides]]\nkey = \"ENG-8\"\ncriticality = \"critical\"\nmodels = [\"haiku\"]\n",
        template()
    );
    let r = PriorityRules::parse(&md).unwrap();
    let e = r.evaluate(&linear_task("eng-9", "x"), Utc::now(), None, 0.0, 0.0);
    assert!(e.skip);
    let e = r.evaluate(&linear_task("ENG-8", "x"), Utc::now(), None, 0.0, 0.0);
    assert_eq!(e.criticality, Criticality::Critical);
    assert_eq!(e.model, Some(ModelTier::haiku()));
}

#[test]
fn model_lists_accept_alternatives_across_providers() {
    let md = format!(
        "{}\n[[overrides]]\nkey = \"ENG-7\"\nmodels = [\"gpt-6-astra\", \"sonnet\"]\n",
        template().replace(r#"critical = ["fable"]"#, r#"critical = ["fable", "gpt-6.1-sol", "opus"]"#)
    );
    let r = PriorityRules::parse(&md).unwrap();
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert_eq!(r.model_for(Criticality::Critical), &[ModelTier::fable(), ModelTier::new("gpt-6.1-sol"), ModelTier::opus()]);
    let mut t = linear_task("ENG-1", "Prod down");
    t.labels = vec!["incident".into()];
    let e = r.evaluate(&t, t.created_at, None, 0.0, 0.0);
    assert_eq!(e.models.len(), 3);
    assert_eq!(e.model, Some(ModelTier::fable()));
    let e = r.evaluate(&linear_task("ENG-7", "x"), Utc::now(), None, 0.0, 0.0);
    assert_eq!(e.models, vec![ModelTier::new("gpt-6-astra"), ModelTier::sonnet()]);
    assert!(e.reasons.iter().any(|x| x == "model gpt-6-astra | sonnet from overrides"), "{:?}", e.reasons);

    // Unknown names fail with the line number and the alias rules.
    let bad = template().replace(r#"high = ["opus"]"#, r#"high = ["opus", "llama"]"#);
    let errs = PriorityRules::parse(&bad).unwrap_err();
    assert_eq!(errs.len(), 1);
    assert!(errs[0].message.contains("unknown model `llama`") && errs[0].message.contains("codex:<name>"), "{}", errs[0].message);
    assert_eq!(bad.lines().nth(errs[0].line - 1).unwrap().trim(), r#"high = ["opus", "llama"]"#);
}

#[test]
fn parse_errors_name_the_line() {
    let md =
        "version = 1\n[[criticality]]\nlevel = \"critical\"\nall = [{ field = \"estimate\", op = \"gt\", value = \"lots\" }]\n";
    let errs = PriorityRules::parse(md).unwrap_err();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0].line, 2);
    assert_eq!(errs[0].to_string(), format!("priority policy line 2: {}", errs[0].message));
}

#[test]
fn condition_matches_is_public() {
    let mut t = linear_task("ENG-1", "Fix the login page");
    t.labels = vec!["Security".into()];
    assert!(condition_matches(&Condition::Equals { field: "label".into(), value: "security".into() }, &t));
    assert!(condition_matches(&Condition::Matches { field: "title".into(), pattern: "login".into() }, &t));
    assert!(!condition_matches(&Condition::GreaterThan { field: "estimate".into(), value: 1.0 }, &t));
}

#[test]
fn synced_issues_are_evaluated_from_stored_fields() {
    let store = Store::open_in_memory().unwrap();
    let issue = LinearIssue {
        id: "u1".into(),
        identifier: "ENG-42".into(),
        title: "Security hole".into(),
        description: "details".into(),
        url: "https://linear.app/x/ENG-42".into(),
        priority: 1,
        estimate: Some(2.0),
        labels: vec!["security".into()],
        state_name: "Todo".into(),
        state_type: "unstarted".into(),
        team_key: "ENG".into(),
        project: None,
        assignee_id: None,
        cycle: None,
        blocked_by: Vec::new(),
        children: Vec::new(),
        parent: None,
        created_at: Utc::now() - Duration::hours(1),
        updated_at: Utc::now(),
    };
    let report = sync_issues(&store, &LinearConfig::default(), &[issue], |_| None).unwrap();
    assert_eq!(report.created.len(), 1);
    let task = store.get_task(report.created[0]).unwrap().unwrap();
    let e = rules().evaluate(&task, Utc::now(), None, 300.0, 2.0);
    assert_eq!(e.criticality, Criticality::Critical);
    assert!(e.score > Criticality::Critical.base_score());
    assert!(e.reasons.iter().any(|r| r.contains("priority: urgent")), "{:?}", e.reasons);
}
