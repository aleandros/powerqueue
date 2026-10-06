//! Captured from a real local Claude Code 2.1.289 session on 2026-10-05:
//! one text-only turn, then --resume with two Read calls. No network or quota
//! is needed to replay it. Message content has been stripped from the fixture.

use std::io::Write;

use powerqueue::domain::{TaskId, TokenUsage};
use powerqueue::session::TranscriptReader;
use powerqueue::store::Store;

#[test]
fn real_claude_session_totals_match_through_incremental_reads_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript.jsonl");
    let sid = uuid::Uuid::new_v4();
    let tid = TaskId::new();
    let store = Store::open_in_memory().unwrap();
    let mut reader = TranscriptReader::new(path.clone(), sid, tid);
    let mut transcript = std::fs::File::create(&path).unwrap();
    let mut inserted = 0;
    // Read between blocks, not just after all usage has arrived.
    for line in include_str!("fixtures/claude-usage-audit.jsonl").lines() {
        writeln!(transcript, "{line}").unwrap();
        for record in reader.read_new().unwrap() {
            inserted += usize::from(store.record_usage(&record).unwrap());
        }
    }
    assert_eq!(inserted, 3, "four content blocks represent three API responses");
    let result: serde_json::Value = serde_json::from_str(include_str!("fixtures/claude-usage-audit-result.json")).unwrap();
    let model = &result["modelUsage"]["claude-sonnet-5-5"];
    let expected = TokenUsage {
        input_tokens: model["inputTokens"].as_u64().unwrap(),
        output_tokens: model["outputTokens"].as_u64().unwrap(),
        cache_creation_input_tokens: model["cacheCreationInputTokens"].as_u64().unwrap(),
        cache_read_input_tokens: model["cacheReadInputTokens"].as_u64().unwrap(),
    };
    assert_eq!(store.usage_for_session(sid).unwrap(), expected);
    assert_eq!(store.usage_for_task(tid).unwrap(), expected);
    assert_eq!(expected.total(), 18_956);
    assert!((expected.weighted() - 17_978.6).abs() < 1e-6);

    // Restarting the daemon replays the file without charging it again.
    let mut restarted = TranscriptReader::new(path, sid, tid);
    for record in restarted.read_new().unwrap() {
        assert!(!store.record_usage(&record).unwrap());
    }
    assert_eq!(store.usage_for_task(tid).unwrap(), expected);
}
