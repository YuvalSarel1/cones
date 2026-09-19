//! Opt-in lifecycle and resource measurements. Only disposable fixtures run.
use super::*;
use std::{fs, mem::size_of, time::Duration};

#[derive(serde::Serialize)]
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
