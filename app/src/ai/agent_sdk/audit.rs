//! Handler for `warp audit ...` commands. PDX-115.
//!
//! The clap argument structures live in [`warp_cli::audit`]; this module
//! owns the actual filesystem / HTTP work so we don't have to drag those
//! dependencies into the lightweight `warp_cli` crate.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use warp_cli::GlobalOptions;
use warp_cli::audit::{
    AuditCommand, AuditEntry, CommonAuditArgs, DEFAULT_AUDIT_LOG_SUFFIX, FollowArgs, Predicate,
    QueryArgs, Summary, SummaryArgs, SyncArgs, parse_line,
};
use warpui::AppContext;

pub fn run(
    _ctx: &mut AppContext,
    _global_options: GlobalOptions,
    command: AuditCommand,
) -> Result<()> {
    match command {
        AuditCommand::Query(args) => query(args),
        AuditCommand::Follow(args) => follow(args),
        AuditCommand::Summary(args) => summary(args),
        AuditCommand::Sync(args) => sync(args),
    }
}

fn resolve_path(args: &CommonAuditArgs) -> Result<PathBuf> {
    if let Some(p) = &args.path {
        return Ok(p.clone());
    }
    let home =
        dirs::home_dir().ok_or_else(|| anyhow!("could not determine $HOME for audit log path"))?;
    Ok(home.join(DEFAULT_AUDIT_LOG_SUFFIX))
}

fn read_all(path: &std::path::Path) -> Result<String> {
    let mut s = String::new();
    File::open(path)
        .with_context(|| format!("opening audit log at {}", path.display()))?
        .read_to_string(&mut s)?;
    Ok(s)
}

fn print_entry(entry: &AuditEntry) -> Result<()> {
    let line = serde_json::to_string(entry)?;
    println!("{line}");
    Ok(())
}

fn query(args: QueryArgs) -> Result<()> {
    let path = resolve_path(&args.common)?;
    let pred = Predicate::from_args(&args.common, Utc::now())?;
    let blob = read_all(&path)?;
    let mut printed = 0usize;
    for (idx, line) in blob.lines().enumerate() {
        let entry = match parse_line(line) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(e) => return Err(e.context(format!("line {}", idx + 1))),
        };
        if !pred.matches(&entry) {
            continue;
        }
        print_entry(&entry)?;
        printed += 1;
        if let Some(limit) = args.limit {
            if printed >= limit {
                break;
            }
        }
    }
    Ok(())
}

fn summary(args: SummaryArgs) -> Result<()> {
    let path = resolve_path(&args.common)?;
    let pred = Predicate::from_args(&args.common, Utc::now())?;
    let blob = read_all(&path)?;
    let entries = warp_cli::audit::parse_jsonl(&blob)?;
    let s = Summary::from_entries(entries.iter(), &pred);
    print!("{}", s.render_table());
    Ok(())
}

fn follow(args: FollowArgs) -> Result<()> {
    let path = resolve_path(&args.common)?;
    let pred = Predicate::from_args(&args.common, Utc::now())?;
    let mut file =
        File::open(&path).with_context(|| format!("opening audit log at {}", path.display()))?;
    file.seek(SeekFrom::End(0))?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            thread::sleep(Duration::from_millis(250));
            continue;
        }
        match parse_line(&buf) {
            Ok(Some(entry)) => {
                if pred.matches(&entry) {
                    print_entry(&entry)?;
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("warp audit follow: skipping malformed line: {e:#}");
            }
        }
    }
}

fn sync(args: SyncArgs) -> Result<()> {
    let path = resolve_path(&args.common)?;
    let pred = Predicate::from_args(&args.common, Utc::now())?;
    let blob = read_all(&path)?;
    let entries: Vec<AuditEntry> = warp_cli::audit::parse_jsonl(&blob)?
        .into_iter()
        .filter(|e| pred.matches(e))
        .collect();
    if entries.is_empty() {
        eprintln!("warp audit sync: no entries to push");
        return Ok(());
    }

    let url = format!("{}/api/audit/sync", args.remote.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut total = 0usize;
    for chunk in entries.chunks(args.batch_size.max(1)) {
        let mut req = client.post(&url).json(&chunk);
        if let Some(token) = &args.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().with_context(|| format!("POST {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(anyhow!("audit-sync failed: {status}: {body}"));
        }
        total += chunk.len();
    }
    eprintln!("warp audit sync: pushed {total} entries to {url}");
    Ok(())
}
