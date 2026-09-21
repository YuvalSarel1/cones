//! Opt-in lifecycle and resource measurements. Only disposable fixtures run.
use super::*;
use std::{fs, mem::size_of, time::Duration};

#[derive(serde::Serialize, Clone)]
struct Resources {
    at_s: f64,
    rss_bytes: u64,
    threads: i32,
    descriptors: usize,
    cpu_ns: u64,
}

fn resources() -> Resources {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_taskinfo>() as i32;
    // sys/proc_info.h: PROC_PIDTASKINFO. Unlike ps, this includes thread count.
    let got = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            4,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    assert_eq!(got, size, "kernel resource read failed");
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
    let ns = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
    Resources {
        at_s: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64(),
        rss_bytes: info.pti_resident_size,
        threads: info.pti_threadnum,
        descriptors: fs::read_dir("/dev/fd").unwrap().count(),
        cpu_ns: ns(usage.ru_utime) + ns(usage.ru_stime),
    }
}

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "lifecycle did not reach its barrier"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "run alone through scripts/check stress; records resource and latency budgets"]
fn repeated_lifecycle_stays_responsive_and_releases_resources() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("claude");
    let project = home.join("projects/fixture");
    fs::create_dir_all(&project).unwrap();
    let path = project.join("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl");
    fs::write(
        &path,
        concat!(
            "{\"type\":\"user\",\"sessionId\":\"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa\",",
            "\"cwd\":\"/fixture\",\"timestamp\":\"2026-09-19T00:00:00Z\",",
            "\"message\":{\"content\":\"resource fixture\"}}\n",
        ),
    )
    .unwrap();
    let cycles = std::env::var("CONES_STRESS_CYCLES")
        .map(|v| v.parse::<usize>().expect("positive cycle count"))
        .unwrap_or(24);
    assert!(cycles >= 8, "include warmup and repeated measurements");
    let started = Instant::now();
    let mut baseline: Option<Resources> = None;
    let mut samples = Vec::new();
    let mut pump_ms: Vec<f64> = Vec::new();
    let mut input_ms: Vec<f64> = Vec::new();
    let mut close_ms: Vec<f64> = Vec::new();
    for cycle in 0..cycles {
        let ack = root.path().join(format!("ack-{cycle}"));
        let mut app = App::new(
            Path::new("cones"),
            &root.path().join("none.yaml"),
            root.path(),
            &home,
        )
        .unwrap();
        // Cancellation while command preparation is blocked cannot launch later.
        let (release, wait) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        app.prepare_viewer("fixture".into(), "cancel".into(), None, None, move || {
            wait.recv_timeout(Duration::from_secs(10)).unwrap();
            done.send(()).unwrap();
            Ok(Command::new("/bin/false"))
        });
        assert!(app.cancel_opening());
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!app.poll_opening() && app.viewers.is_empty());

        let ack_path = ack.clone();
        app.prepare_viewer("fixture".into(), "flood".into(), None, None, move || {
            let mut command = Command::new("/usr/bin/python3");
            command
                .args([
                    "-c",
                    r#"
import os, pathlib, select, sys, tty
tty.setraw(0)
os.write(1, b"READY\r\n")
while True:
    if select.select([0], [], [], 0)[0]:
        pathlib.Path(sys.argv[1]).write_bytes(os.read(0, 1))
    os.write(1, b"x" * 8192)
"#,
                ])
                .arg(ack_path);
            Ok(command)
        });
        until(|| app.poll_opening());
        assert_eq!(app.viewers.len(), 1, "{}", app.status);
        let pid = app.viewers[0].viewer.pid();
        app.focus = Some(0);
        until(|| {
            let before = Instant::now();
            app.pump();
            pump_ms.push(before.elapsed().as_secs_f64() * 1000.0);
            app.viewers[0].viewer.first_paint().is_some()
        });
        let before = Instant::now();
        app.key(KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        until(|| {
            let pumping = Instant::now();
            app.pump();
            pump_ms.push(pumping.elapsed().as_secs_f64() * 1000.0);
            fs::read(&ack).is_ok_and(|bytes| bytes == b"x")
        });
        input_ms.push(before.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(fs::read(&ack).unwrap(), b"x");
        let closing = Instant::now();
        app.close(0);
        assert!(app.viewers.is_empty());
        until(|| unsafe { libc::kill(pid as i32, 0) } != 0);
        close_ms.push(closing.elapsed().as_secs_f64() * 1000.0);
        drop(app);

        // Refresh and discard both worker types repeatedly, including their caches.
        let mut history = history::Reader::new(vec![history::Source {
            harness: HarnessKind::Claude,
            home: home.clone(),
        }])
        .unwrap();
        history
            .request(history::Query {
                hydrate: true,
                refresh: true,
                ..Default::default()
            })
            .unwrap();
        until(|| match history.poll() {
            Some(result) => {
                assert_eq!(result.unwrap().total, 1);
                true
            }
            None => false,
        });
        let mut preview = transcript::Reader::new().unwrap();
        preview
            .request(transcript::Target {
                key: "fixture".into(),
                harness: "claude".into(),
                source: transcript::Source::Conversation(path.clone()),
            })
            .unwrap();
        until(|| match preview.poll() {
            Some(result) => {
                assert!(!result.unwrap().result.unwrap().messages.is_empty());
                true
            }
            None => false,
        });
        drop((preview, history));
        if let Some(base) = &baseline {
            until(|| resources().threads <= base.threads);
        }
        let sample = resources();
        if cycle == 3 {
            baseline = Some(sample);
        } else if cycle > 3 {
            samples.push(sample);
        }
    }
    let baseline = baseline.unwrap();
    let final_sample = resources();
    let max_pump = pump_ms.iter().copied().fold(0.0_f64, f64::max);
    let max_input = input_ms.iter().copied().fold(0.0_f64, f64::max);
    let max_close = close_ms.iter().copied().fold(0.0_f64, f64::max);
    let cpu_cores = (final_sample.cpu_ns - baseline.cpu_ns) as f64
        / ((final_sample.at_s - baseline.at_s) * 1e9);
    let report = json!({
        "cycles": cycles, "elapsed_s": started.elapsed().as_secs_f64(),
        "baseline_after_warmup": baseline, "samples": samples,
        "final": final_sample, "pump_max_ms": max_pump, "input_max_ms": max_input,
        "average_cpu_cores_after_warmup": cpu_cores,
        "close_max_ms": max_close,
        "scope": "fixture dashboard process; no native harness or model; CPU is cumulative"
    });
    if let Some(path) = std::env::var_os("CONES_STRESS_OUTPUT") {
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    println!("{report}");
    assert!(
        max_pump < 500.0 && max_input < 1000.0,
        "flood starved input: {report}"
    );
    assert!(max_close < 1000.0, "closing stalled: {report}");
    assert!(
        samples
            .iter()
            .all(|s| s.descriptors <= baseline.descriptors && s.threads <= baseline.threads),
        "resources accumulated: {report}"
    );
    // Allocators retain warm buffers; sustained growth beyond this is a regression.
    assert!(
        final_sample.rss_bytes <= baseline.rss_bytes + 16 * 1024 * 1024,
        "resident memory grew: {report}"
    );
}

/// Sustained populated discovery across concurrent dashboards.
///
/// The lifecycle fixture above measures a dashboard that opens and closes viewers. This one
/// measures the cost the dashboard pays when it is only watching: several dashboards refreshing
/// a populated fleet for as long as the caller asks, with each refresh's own observation
/// accounting checked against a fixed budget rather than a wall-clock guess.
///
/// `CONES_STRESS_SECONDS` sets the duration; the soak the repair was accepted against is
/// `CONES_STRESS_SECONDS=3600`, which spans the interval the machine wedged over.
/// `CONES_STRESS_DASHBOARDS` sets how many refresh at once.
#[test]
#[ignore = "run alone through scripts/check stress; records the observation budget"]
fn sustained_discovery_across_dashboards_stays_within_its_observation_budget() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("claude");
    let registry = home.join("sessions");
    fs::create_dir_all(&registry).unwrap();
    // The fixture's rows must be live, or discovery drops them and measures an empty fleet.
    // This process is the one pid a test can be sure of, and its start is what the registry
    // has to agree with.
    let pid = std::process::id();
    let start = crate::fleet::starts(&[u64::from(pid)])
        .unwrap()
        .remove(&pid)
        .expect("this process is in the table");
    let folders: Vec<PathBuf> = (0..3)
        .map(|i| {
            let dir = root.path().join(format!("repo-{i}"));
            fs::create_dir_all(dir.join("sub")).unwrap();
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["init", "-b", "main"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            dir
        })
        .collect();
    let rows = 12;
    for i in 0..rows {
        let id = format!("aaaaaaaa-aaaa-4aaa-8aaa-{i:012}");
        fs::write(
            registry.join(format!("{id}.json")),
            json!({
                "pid": pid, "sessionId": id, "procStart": start,
                "cwd": folders[i % folders.len()].join("sub"),
                "kind": "bg", "status": "idle",
            })
            .to_string(),
        )
        .unwrap();
    }
    let jobs = root.path().join("jobs.yaml");
    fs::write(
        &jobs,
        format!(
            "cones:\n  columns: [state, title, folder, branch, cpu]\n  folders:\n{}",
            folders
                .iter()
                .map(|f| format!("    - {}\n", f.display()))
                .collect::<String>()
        ),
    )
    .unwrap();

    let seconds: u64 =
        std::env::var("CONES_STRESS_SECONDS").map_or(20, |v| v.parse().expect("whole seconds"));
    let dashboards: usize = std::env::var("CONES_STRESS_DASHBOARDS")
        .map_or(3, |v| v.parse().expect("a dashboard count"));
    // Unpaced by default, which is the budget check: every refresh is measured back to back.
    // A soak asks for the cadence a dashboard actually refreshes at, because what it measures
    // is what accumulates over an hour, not what a burst costs.
    let interval = Duration::from_millis(
        std::env::var("CONES_STRESS_INTERVAL_MS").map_or(0, |v| v.parse().expect("milliseconds")),
    );
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let baseline = resources();
    let budget = 3 + folders.len() as u64;
    let reports: Vec<_> = (0..dashboards)
        .map(|_| {
            let (jobs, state, home) = (jobs.clone(), root.path().to_owned(), home.clone());
            std::thread::spawn(move || {
                let (mut passes, mut worst) = (0u64, 0.0_f64);
                let mut total = BTreeMap::<&'static str, u64>::new();
                while Instant::now() < deadline {
                    // An idle stretch between refreshes is part of what a soak alternates.
                    std::thread::sleep(interval);
                    let at = Instant::now();
                    let data = Data::load(&jobs, &state, &home).unwrap();
                    worst = worst.max(at.elapsed().as_secs_f64() * 1000.0);
                    passes += 1;
                    assert!(
                        data.sessions.len() >= rows,
                        "the fixture fleet emptied: {} rows",
                        data.sessions.len()
                    );
                    let counts = crate::observe::snapshot();
                    let spawns = |op| {
                        counts
                            .get(op)
                            .map_or(0, |c: &crate::observe::Counts| c.spawns)
                    };
                    assert_eq!(
                        spawns(crate::observe::op::PROCESS_TABLE),
                        1,
                        "one whole-table read a refresh, whatever the harness count"
                    );
                    assert_eq!(spawns(crate::observe::op::SQLITE), 0);
                    assert_eq!(spawns(crate::observe::op::LIVENESS), 0);
                    assert!(
                        crate::observe::spawns() <= budget,
                        "a refresh went over its process budget: {counts:?}"
                    );
                    for (op, c) in counts {
                        *total.entry(op).or_default() += c.spawns;
                    }
                }
                (passes, worst, total)
            })
        })
        .collect::<Vec<_>>();
    // Sample while they run, so the report shows the shape of the hour rather than only its
    // ends: a warm plateau and a slow leak have the same first and last reading.
    let mut samples = vec![baseline.clone()];
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1).min(deadline - Instant::now()));
        if samples
            .last()
            .is_some_and(|s: &Resources| resources().at_s - s.at_s >= 60.0)
        {
            samples.push(resources());
        }
    }
    let reports: Vec<_> = reports.into_iter().map(|h| h.join().unwrap()).collect();
    let final_sample = resources();
    let passes: u64 = reports.iter().map(|r| r.0).sum();
    let spawns: u64 = reports
        .iter()
        .flat_map(|r| r.2.values())
        .copied()
        .sum::<u64>();
    let mut by_operation = BTreeMap::<&'static str, u64>::new();
    for (_, _, counts) in &reports {
        for (op, n) in counts {
            *by_operation.entry(op).or_default() += n;
        }
    }
    let report = json!({
        "dashboards": dashboards, "rows": rows, "folders": folders.len(),
        "seconds": started.elapsed().as_secs_f64(), "passes": passes,
        "spawns": spawns,
        "spawns_per_pass": spawns as f64 / passes as f64,
        "spawns_per_second": spawns as f64 / started.elapsed().as_secs_f64(),
        "by_operation": by_operation,
        "refresh_max_ms": reports.iter().fold(0.0_f64, |m, r| m.max(r.1)),
        "baseline_after_warmup": baseline, "samples": samples, "final": final_sample,
        "average_cpu_cores": (final_sample.cpu_ns - baseline.cpu_ns) as f64
            / ((final_sample.at_s - baseline.at_s) * 1e9),
        "scope": "fixture dashboards in one process; cones-owned observation only"
    });
    if let Some(path) = std::env::var_os("CONES_STRESS_OBSERVATION_OUTPUT") {
        fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    println!("{report}");
    assert!(passes >= dashboards as u64, "every dashboard refreshed");
    assert!(
        final_sample.descriptors <= baseline.descriptors + 8
            && final_sample.threads <= baseline.threads + dashboards as i32,
        "observation retained descriptors or threads: {report}"
    );
    assert!(
        final_sample.rss_bytes <= baseline.rss_bytes + 64 * 1024 * 1024,
        "resident memory grew while only observing: {report}"
    );
}
