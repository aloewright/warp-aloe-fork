//! Unit tests for the [`crate::audit`] JSONL parser + filter predicates.

use super::*;
use chrono::TimeZone;

const SAMPLE: &str = r#"{"timestamp":"2026-05-01T00:00:00Z","task_id":"t1","agent_id":"a1","rule":"diff_size","action":"blocked","offending_path":"src/main.rs","detail":"too big"}
{"timestamp":"2026-05-01T01:00:00Z","task_id":"t2","agent_id":"a2","rule":"budget_exceeded","action":"overridden","offending_path":"","detail":""}

{"timestamp":"2026-05-01T02:00:00Z","task_id":"t3","agent_id":"a1","rule":"deploy_gate","action":"allowed","offending_path":"","detail":"approved"}
"#;

fn args() -> CommonAuditArgs {
    CommonAuditArgs {
        path: None,
        action: None,
        rule: None,
        actor: None,
        since: None,
        filter: vec![],
    }
}

#[test]
fn parse_line_skips_blanks() {
    assert!(parse_line("").unwrap().is_none());
    assert!(parse_line("   \t  ").unwrap().is_none());
}

#[test]
fn parse_line_parses_full_row() {
    let entry = parse_line(SAMPLE.lines().next().unwrap()).unwrap().unwrap();
    assert_eq!(entry.task_id, "t1");
    assert_eq!(entry.agent_id, "a1");
    assert_eq!(entry.rule, "diff_size");
    assert_eq!(entry.action, "blocked");
    assert_eq!(entry.offending_path, "src/main.rs");
    assert_eq!(entry.detail, "too big");
}

#[test]
fn parse_jsonl_handles_blank_lines() {
    let entries = parse_jsonl(SAMPLE).unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].task_id, "t1");
    assert_eq!(entries[2].task_id, "t3");
}

#[test]
fn parse_jsonl_surfaces_line_number_on_error() {
    let bad = "{\"timestamp\":\"x\"}\nnot-json\n";
    let err = parse_jsonl(bad).unwrap_err();
    assert!(format!("{err:#}").contains("line 2"));
}

#[test]
fn parse_preserves_unknown_fields() {
    let line =
        r#"{"timestamp":"2026-05-01T00:00:00Z","action":"x","custom":42,"nested":{"k":"v"}}"#;
    let entry = parse_line(line).unwrap().unwrap();
    assert_eq!(entry.action, "x");
    assert_eq!(
        entry.extra.get("custom").and_then(|v| v.as_u64()),
        Some(42)
    );
    assert!(entry.extra.contains_key("nested"));
}

#[test]
fn parsed_timestamp_handles_rfc3339() {
    let line = r#"{"timestamp":"2026-05-01T12:34:56Z","action":"x"}"#;
    let entry = parse_line(line).unwrap().unwrap();
    let ts = entry.parsed_timestamp().unwrap();
    assert_eq!(ts, Utc.with_ymd_and_hms(2026, 5, 1, 12, 34, 56).unwrap());
}

#[test]
fn parsed_timestamp_returns_none_on_garbage() {
    let entry = parse_line(r#"{"timestamp":"not-a-time","action":"x"}"#)
        .unwrap()
        .unwrap();
    assert!(entry.parsed_timestamp().is_none());
}

#[test]
fn predicate_action_filter() {
    let entries = parse_jsonl(SAMPLE).unwrap();
    let mut a = args();
    a.action = Some("blocked".into());
    let pred = Predicate::from_args(&a, Utc::now()).unwrap();
    let matched: Vec<_> = entries.iter().filter(|e| pred.matches(e)).collect();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].task_id, "t1");
}

#[test]
fn predicate_rule_filter_via_kv() {
    let entries = parse_jsonl(SAMPLE).unwrap();
    let mut a = args();
    a.filter = vec!["rule=budget_exceeded".into()];
    let pred = Predicate::from_args(&a, Utc::now()).unwrap();
    let matched: Vec<_> = entries.iter().filter(|e| pred.matches(e)).collect();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].task_id, "t2");
}

#[test]
fn predicate_actor_matches_either_id() {
    let entries = parse_jsonl(SAMPLE).unwrap();
    let mut a = args();
    a.actor = Some("a1".into());
    let pred = Predicate::from_args(&a, Utc::now()).unwrap();
    let matched: Vec<_> = entries.iter().filter(|e| pred.matches(e)).collect();
    assert_eq!(matched.len(), 2); // t1 + t3 both have agent_id=a1
}

#[test]
fn predicate_since_window() {
    let entries = parse_jsonl(SAMPLE).unwrap();
    // Reference time: 2026-05-01T03:00:00Z. With --since=90m we should see
    // only t3 (02:00) and t2 (01:00 is on the boundary; 03:00 - 90m = 01:30).
    let now = Utc.with_ymd_and_hms(2026, 5, 1, 3, 0, 0).unwrap();
    let mut a = args();
    a.since = Some("90m".parse().unwrap());
    let pred = Predicate::from_args(&a, now).unwrap();
    let matched: Vec<_> = entries.iter().filter(|e| pred.matches(e)).collect();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].task_id, "t3");
}

#[test]
fn predicate_rejects_unknown_filter_key() {
    let mut a = args();
    a.filter = vec!["fnord=1".into()];
    let err = Predicate::from_args(&a, Utc::now()).unwrap_err();
    assert!(format!("{err:#}").contains("unknown --filter key"));
}

#[test]
fn predicate_rejects_malformed_filter() {
    let mut a = args();
    a.filter = vec!["norule".into()];
    let err = Predicate::from_args(&a, Utc::now()).unwrap_err();
    assert!(format!("{err:#}").contains("KEY=VALUE"));
}

#[test]
fn summary_groups_by_action_rule_actor() {
    let entries = parse_jsonl(SAMPLE).unwrap();
    let pred = Predicate::default();
    let s = Summary::from_entries(entries.iter(), &pred);
    assert_eq!(s.total, 3);
    assert_eq!(s.by_action.get("blocked"), Some(&1));
    assert_eq!(s.by_action.get("overridden"), Some(&1));
    assert_eq!(s.by_action.get("allowed"), Some(&1));
    assert_eq!(s.by_rule.get("budget_exceeded"), Some(&1));
    // a1 appears in two rows (t1 + t3).
    assert_eq!(s.by_actor.get("a1"), Some(&2));
    assert_eq!(s.by_actor.get("a2"), Some(&1));
}

#[test]
fn summary_respects_predicate() {
    let entries = parse_jsonl(SAMPLE).unwrap();
    let mut a = args();
    a.actor = Some("a1".into());
    let pred = Predicate::from_args(&a, Utc::now()).unwrap();
    let s = Summary::from_entries(entries.iter(), &pred);
    assert_eq!(s.total, 2);
    assert!(s.by_actor.get("a2").is_none());
}

#[test]
fn render_table_smoke() {
    let entries = parse_jsonl(SAMPLE).unwrap();
    let s = Summary::from_entries(entries.iter(), &Predicate::default());
    let rendered = s.render_table();
    assert!(rendered.contains("total: 3"));
    assert!(rendered.contains("By action:"));
    assert!(rendered.contains("blocked"));
}
