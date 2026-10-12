//! One-way migration of the retired Markdown grammar. Runtime never calls this parser.

use super::{PriorityRules, RuleError};

/// Convert the actual legacy grammar to strict TOML, preserving evaluation
/// semantics. Returns legacy warnings (ignored prose is not executable), or
/// errors if either the old document or the generated policy is invalid.
pub fn migrate(markdown: &str) -> anyhow::Result<(String, Vec<RuleError>)> {
    let rules =
        PriorityRules::parse_legacy(markdown).map_err(|e| anyhow::anyhow!("legacy Markdown: {}", super::format_errors(&e)))?;
    let text = super::policy::serialize(&rules)?;
    PriorityRules::parse(&text).map_err(|e| anyhow::anyhow!("migration requires manual repair: {}", super::format_errors(&e)))?;
    Ok((text, rules.warnings))
}

#[cfg(test)]
mod properties {
    use super::*;
    use crate::strategies as arb;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn migrated_policies_preserve_evaluation(task in arb::policy_task(), now in arb::instant(), jev in 0.0f64..1.0, age in 0.0f64..10.0) {
            for text in [include_str!("../../tests/fixtures/priority/legacy-template.md"), include_str!("../../tests/fixtures/priority/legacy.md")] {
                let old = PriorityRules::parse_legacy(text).unwrap();
                let (toml, _) = migrate(text).unwrap();
                let new = PriorityRules::parse(&toml).unwrap();
                let a = old.evaluate(&task, now, Some(jev), 300.0, age);
                let b = new.evaluate(&task, now, Some(jev), 300.0, age);
                // Locations/explanations refer to the new file; all decisions stay the same.
                prop_assert_eq!((a.criticality, a.score, a.models, a.skip), (b.criticality, b.score, b.models, b.skip));
            }
        }
    }

    #[test]
    fn migration_reports_legacy_warnings_and_never_interprets_prose() {
        let (toml, warnings) = migrate(include_str!("../../tests/fixtures/priority/legacy.md")).unwrap();
        assert!(warnings.iter().any(|w| w.message.contains("first matching")));
        assert!(warnings.iter().any(|w| w.message.contains("last one wins")));
        assert!(warnings.iter().any(|w| w.message.contains("unknown section")));
        assert!(!toml.contains("hidden"));
        assert_eq!(PriorityRules::parse(&migrate("Run urgent tasks first.").unwrap().0).unwrap().rule_count(), 0);
    }

    #[test]
    fn legacy_and_inside_quotes_is_not_silently_reinterpreted() {
        assert!(migrate("## High\n- title: \"research and development\"\n").is_err());
        let new = PriorityRules::parse("version = 1\n[[criticality]]\nlevel = 'high'\nall = [{field = 'title', op = 'eq', value = 'research and development'}]\n").unwrap();
        let task = crate::domain::Task::new("T", "research and development", crate::domain::TaskSource::Manual);
        assert_eq!(new.evaluate(&task, task.created_at, None, 0.0, 0.0).criticality, crate::domain::Criticality::High);
    }

    #[test]
    fn migrated_policy_can_add_previously_absent_rule_tables() {
        let (mut text, _) = migrate("## High\n- source: manual\n").unwrap();
        text.push_str("\n[[overrides]]\nkey = 'NEW-1'\nskip = true\n");
        text.push_str("\n[[model_rules]]\nmodels = ['sonnet']\nall = [{field = 'source', op = 'eq', value = 'manual'}]\n");
        assert!(PriorityRules::parse(&text).is_ok());
    }

    #[test]
    fn nonfinite_legacy_values_require_manual_repair() {
        assert!(migrate("## Overrides\n- A: +inf\n").unwrap_err().to_string().contains("manual repair"));
    }
}
