//! Every registered native transcript reader must price the same usage through its live and history readers.
//! This binary alone owns its in-process price service; no harness or model is started.
use chrono::{DateTime, Utc};
use cones::{codex, config::HarnessKind, cost, fleet, harness, history, opencode, pi};
use serde_json::json;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const AT: &str = "2026-09-16T10:00:00Z";

struct Fixture {
    kind: HarnessKind,
    home: PathBuf,
    path: PathBuf,
}

fn sql(db: &Path, text: &str) {
    let mut child = Command::new("sqlite3")
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

impl Fixture {
    fn new(root: &Path, kind: HarnessKind) -> Self {
        let home = root.join(kind.to_string());
        // Exhaustive: adding a HarnessKind requires a native accounting fixture.
        let path = home.join(match kind {
            HarnessKind::Claude => format!("projects/-fixture/{ID}.jsonl"),
            HarnessKind::Codex => format!("sessions/2026/09/16/rollout-{ID}.jsonl"),
            HarnessKind::Pi => format!("sessions/--fixture--/{ID}.jsonl"),
            HarnessKind::Opencode => "opencode.db".into(),
            HarnessKind::Gemini
            | HarnessKind::Cursor
            | HarnessKind::Copilot
            | HarnessKind::Amp
            | HarnessKind::Droid
            | HarnessKind::Kimi => unreachable!("terminal-only capabilities checked separately"),
        });
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if kind == HarnessKind::Claude {
            let registry = home.join("sessions");
            fs::create_dir_all(&registry).unwrap();
            fs::write(
                registry.join("client.json"),
                json!({
                    "pid": std::process::id(), "sessionId": ID, "cwd":"/fixture",
                    "kind":"interactive", "status":"idle"
                })
                .to_string(),
            )
            .unwrap();
        }
        if kind == HarnessKind::Opencode {
            sql(
                &path,
                include_str!("../assets/harnesses/fixtures/opencode.sql"),
            );
            sql(
                &path,
                "DELETE FROM session WHERE id != 'ses_fixture'; DELETE FROM message; DELETE FROM part; UPDATE session SET cost = NULL;",
            );
        }
        let fixture = Self { kind, home, path };
        fixture.write(false, false);
        fixture
    }

    fn write(&self, gap: bool, native_response: bool) {
        let mut records = match self.kind {
            HarnessKind::Claude => vec![json!({
                "type":"user","sessionId":ID,"cwd":"/fixture","timestamp":AT,
                "message":{"content":"Check accounting"}
            })],
            HarnessKind::Codex => vec![json!({
                "type":"session_meta","timestamp":AT,
                "payload":{"session_id":ID,"cwd":"/fixture","timestamp":AT,"model_provider":"provider"}
            })],
            HarnessKind::Pi => vec![json!({
                "type":"session","id":ID,"cwd":"/fixture","timestamp":AT
            })],
            HarnessKind::Gemini
            | HarnessKind::Cursor
            | HarnessKind::Copilot
            | HarnessKind::Amp
            | HarnessKind::Droid
            | HarnessKind::Kimi => unreachable!("no native archive"),
            HarnessKind::Opencode => {
                sql(
                    &self.path,
                    "DELETE FROM message; UPDATE session SET cost = NULL;",
                );
                Vec::new()
            }
        };
        for n in 1..=if gap { 2 } else { 1 } {
            let model = if n == 1 { "model" } else { "unknown" };
            let native = if n == 2 && native_response { 0.2 } else { 0.0 };
            let id = format!("message-{n}");
            let event = match self.kind {
                HarnessKind::Gemini
                | HarnessKind::Cursor
                | HarnessKind::Copilot
                | HarnessKind::Amp
                | HarnessKind::Droid
                | HarnessKind::Kimi => unreachable!("no native archive"),
                HarnessKind::Claude => json!({
                    "type":"assistant","timestamp":AT,
                    "message":{"id":id,"provider":"provider","model":model,
                        "usage":{"input_tokens":30,"cache_read_input_tokens":40,
                            "cache_creation_input_tokens":30,"output_tokens":10},
                        "content":[{"type":"text","text":"Done"}]}
                }),
                HarnessKind::Codex => {
                    records.push(json!({"type":"turn_context","payload":{"model":model}}));
                    json!({"type":"event_msg","timestamp":AT,"payload":{
                        "type":"token_count","info":{
                            "last_token_usage":{"input_tokens":100,"cached_input_tokens":40,
                                "cache_write_input_tokens":30,"output_tokens":10},
                            "total_token_usage":{"input_tokens":100*n,"cached_input_tokens":40*n,
                                "cache_write_input_tokens":30*n,"output_tokens":10*n}
                        }
                    }})
                }
                HarnessKind::Pi => json!({
                    "type":"message","id":id,"timestamp":AT,"message":{
                        "role":"assistant","provider":"provider","model":model,
                        "usage":{"input":30,"cacheRead":40,"cacheWrite":30,"output":10,
                            "cost":{"total":native}},
                        "content":[{"type":"text","text":"Done"}],"stopReason":"stop"
                    }
                }),
                HarnessKind::Opencode => json!({
                    "role":"assistant","providerID":"provider","modelID":model,
                    "tokens":{"input":30,"cache":{"read":40,"write":30},"output":8,"reasoning":2},
                    "cost":native
                }),
            };
            if self.kind == HarnessKind::Opencode {
                sql(
                    &self.path,
                    &format!(
                        "INSERT INTO message VALUES ('{id}', 'ses_fixture', {n}, '{}');",
                        event.to_string().replace('\'', "''")
                    ),
                );
            } else {
                records.push(event.clone());
                records.push(event); // Streaming repeats must not multiply dollars or gaps.
            }
        }
        if self.kind != HarnessKind::Opencode {
            fs::write(
                &self.path,
                records.iter().map(|v| format!("{v}\n")).collect::<String>(),
            )
            .unwrap();
        }
    }

    fn live(&self) -> fleet::Session {
        let started = AT.parse::<DateTime<Utc>>().unwrap() - Duration::from_secs(1);
        let pid = std::process::id();
        let cwd = Some(PathBuf::from("/fixture"));
        let rows = match self.kind {
            HarnessKind::Gemini
            | HarnessKind::Cursor
            | HarnessKind::Copilot
            | HarnessKind::Amp
            | HarnessKind::Droid
            | HarnessKind::Kimi => unreachable!("native usage is unavailable"),
            HarnessKind::Claude => fleet::sessions(&self.home).unwrap(),
            HarnessKind::Codex => codex::rows(
                &self.home,
                &[codex::Process {
                    pid,
                    started,
                    cwd,
                    thread: Some(ID.into()),
                    remote: false,
                    prompt: None,
                }],
            ),
            HarnessKind::Pi => pi::rows(&self.home, &[pi::Process { pid, started, cwd }]),
            HarnessKind::Opencode => opencode::rows(
                &self.home,
                &[opencode::Process {
                    pid,
                    started,
                    cwd,
                    session_id: Some("ses_fixture".into()),
                }],
            )
            .unwrap(),
        };
        assert_eq!(rows.len(), 1, "{}", self.kind);
        rows.into_iter().next().unwrap()
    }

    fn native_total(&self, usd: f64) {
        match self.kind {
            HarnessKind::Claude => {
                fs::create_dir_all(self.home.join("statusline")).unwrap();
                fs::write(
                    self.home.join(format!("statusline/{ID}.json")),
                    json!({"cost":{"total_cost_usd":usd}}).to_string(),
                )
                .unwrap();
            }
            HarnessKind::Opencode => sql(&self.path, &format!("UPDATE session SET cost = {usd};")),
            HarnessKind::Pi
            | HarnessKind::Codex
            | HarnessKind::Gemini
            | HarnessKind::Cursor
            | HarnessKind::Copilot
            | HarnessKind::Amp
            | HarnessKind::Droid
            | HarnessKind::Kimi => unreachable!("no native session total"),
        }
    }
}

fn page(reader: &mut history::Reader, refresh: bool) -> history::Page {
    assert!(
        reader
            .request(history::Query {
                hydrate: true,
                refresh,
                ..Default::default()
            })
            .unwrap()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(page) = reader.poll() {
            return page.unwrap();
        }
        assert!(Instant::now() < deadline, "history worker stalled");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn assert_cost(
    fixture: &Fixture,
    reader: &mut history::Reader,
    refresh: bool,
    expected: Option<f64>,
    coverage: cost::Coverage,
) -> history::Page {
    let live = fixture.live();
    let page = page(reader, refresh);
    assert_eq!(page.entries.len(), 1, "{}", fixture.kind);
    let columns = page.entries[0].columns.as_ref().unwrap();
    assert_eq!(live.cost_usd, columns.cost_usd, "{}", fixture.kind);
    assert_eq!(live.cost_info, columns.cost_info, "{}", fixture.kind);
    match (columns.cost_usd, expected) {
        (Some(actual), Some(expected)) => assert!(
            (actual - expected).abs() < 1e-12,
            "{}: {actual} != {expected}",
            fixture.kind
        ),
        (actual, expected) => assert_eq!(actual, expected, "{}", fixture.kind),
    }
    assert_eq!(
        columns.cost_info.as_ref().unwrap().coverage,
        coverage,
        "{}",
        fixture.kind
    );
    page
}

fn prices(state: &Path, factor: f64, expired: bool) {
    let mut catalog = cost::Catalog::parse(&serde_json::to_vec(&json!({
        "provider":{"models":{"model":{"cost":{
            "input":2.0*factor,"output":8.0*factor,"cache_read":0.5*factor,"cache_write":3.0*factor
        }}}}
    })).unwrap(), Utc::now()).unwrap();
    if expired {
        catalog.stamp.fetched_at -= chrono::Duration::days(8);
    }
    fs::create_dir_all(state).unwrap();
    fs::write(
        state.join("prices.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    cost::init(state, false);
}

#[test]
fn every_registered_harness_inherits_fallback_precedence_coverage_and_cache_refresh() {
    use cost::Coverage::{Complete, Partial, Unavailable};
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("prices");
    let missing = dir.path().join("missing");
    for &kind in harness::known() {
        if !harness::spec(kind).transcript.available {
            continue;
        }
        let fixture = Fixture::new(dir.path(), kind);
        let mut reader = history::Reader::new(vec![history::Source {
            harness: kind,
            home: fixture.home.clone(),
        }])
        .unwrap();
        cost::init(&missing, false);
        assert_cost(&fixture, &mut reader, false, None, Unavailable);
        prices(&state, 1.0, false);
        let priced = assert_cost(&fixture, &mut reader, false, Some(0.00025), Complete);
        let info = priced.entries[0]
            .columns
            .as_ref()
            .unwrap()
            .cost_info
            .as_ref()
            .unwrap();
        assert_eq!(info.source, cost::Source::ModelsDev, "{kind}");
        assert_eq!(info.priced_records, 1, "{kind}: duplicate response");
        assert_eq!(priced.stats.hydrated_files, 1, "{kind}: catalog arrival");
        assert_eq!(page(&mut reader, false).stats.column_cache_hits, 1);
        prices(&state, 2.0, false);
        assert_eq!(
            assert_cost(&fixture, &mut reader, false, Some(0.0005), Complete)
                .stats
                .hydrated_files,
            1,
            "{kind}: new rates"
        );
        prices(&state, 2.0, true);
        assert_eq!(
            assert_cost(&fixture, &mut reader, false, None, Unavailable)
                .stats
                .hydrated_files,
            1,
            "{kind}: expiry"
        );
        prices(&state, 1.0, false);
        fixture.write(true, false);
        let partial = assert_cost(&fixture, &mut reader, true, Some(0.00025), Partial);
        let info = partial.entries[0]
            .columns
            .as_ref()
            .unwrap()
            .cost_info
            .as_ref()
            .unwrap();
        assert_eq!(
            (info.priced_records, info.unpriced_records),
            (1, 1),
            "{kind}"
        );
        match kind {
            HarnessKind::Gemini
            | HarnessKind::Cursor
            | HarnessKind::Copilot
            | HarnessKind::Amp
            | HarnessKind::Droid
            | HarnessKind::Kimi => unreachable!("no native archive"),
            HarnessKind::Claude | HarnessKind::Opencode => {
                fixture.native_total(0.9);
                let native = assert_cost(&fixture, &mut reader, true, Some(0.9), Complete);
                assert_eq!(
                    native.entries[0]
                        .columns
                        .as_ref()
                        .unwrap()
                        .cost_info
                        .as_ref()
                        .unwrap()
                        .source,
                    cost::Source::Harness
                );
                fixture.native_total(0.0);
                assert_cost(&fixture, &mut reader, true, Some(0.0), Complete);
            }
            HarnessKind::Pi | HarnessKind::Codex => {}
        }
        if matches!(kind, HarnessKind::Pi | HarnessKind::Opencode) {
            fixture.write(true, true);
            assert_cost(&fixture, &mut reader, true, Some(0.20025), Complete);
            cost::init(&missing, false);
            assert_cost(&fixture, &mut reader, false, Some(0.2), Partial);
        }
    }
}
