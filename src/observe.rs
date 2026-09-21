//! Accounting for cones' own observation work.
//!
//! A refresh runs on its own thread, so the counters are thread-local: one thread is one pass.
//! Background workers keep their own attribution, parallel tests cannot see each other's counts,
//! and the machine's live dashboards never contribute to a test's numbers.
//!
//! This is diagnostic accounting, not a permission engine and not a run budget. It covers the
//! commands cones launches to observe; it never wraps a harness's own execution.
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    process::{Command, Output},
    time::Instant,
};

/// Fixed operation categories. Routine accounting must not retain a map of paths, so an
/// operation is one of these names and nothing finer; a controlled investigation traces calls.
pub mod op {
    /// The whole process table, the read every native adapter shares within a pass.
    pub const PROCESS_TABLE: &str = "process_table";
    /// A batched environment read for named pids, to attribute a client to a native home.
    pub const PROCESS_ENV: &str = "process_env";
    /// A batched start-time read for named pids, which rejects a reused pid.
    pub const PROCESS_START: &str = "process_start";
    /// A batched cpu and resident-size read for named pids.
    pub const PROCESS_USAGE: &str = "process_usage";
    /// Working directories and held locks read from the kernel, or from `lsof` when it refuses.
    pub const OPEN_FILES: &str = "open_files";
    /// Whether one pid exists.
    pub const LIVENESS: &str = "liveness";
    /// Repository layout for a folder the list can name.
    pub const GIT: &str = "git";
    /// A native index or state database.
    pub const SQLITE: &str = "sqlite";
}

/// What one category cost in one pass.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Counts {
    /// Subprocesses this pass launched, whether or not they succeeded.
    pub spawns: u64,
    /// Source reads performed in process, such as a database query or a kernel call.
    pub reads: u64,
    /// Attempts that failed or could not run.
    pub failures: u64,
    /// Acquisitions answered from a result the pass already held.
    pub shared: u64,
    /// Total wall time attributed to the category.
    pub ms: f64,
}

thread_local! {
    static PASS: Cell<u64> = const { Cell::new(0) };
    static COUNTS: RefCell<BTreeMap<&'static str, Counts>> = const { RefCell::new(BTreeMap::new()) };
}

fn with(op: &'static str, f: impl FnOnce(&mut Counts)) {
    COUNTS.with(|c| f(c.borrow_mut().entry(op).or_default()));
}

/// Launch an observation subprocess and count it. Failure to spawn is counted too: a budget
/// that only sees successful spawns hides the case where process creation is exhausted.
pub fn spawn(op: &'static str, command: &mut Command) -> std::io::Result<Output> {
    let started = Instant::now();
    let out = command.output();
    with(op, |c| {
        c.spawns += 1;
        c.ms += started.elapsed().as_secs_f64() * 1000.0;
        if !out.as_ref().is_ok_and(|o| o.status.success()) {
            c.failures += 1;
        }
    });
    out
}

/// Count an in-process source read, such as a database query or a kernel process call.
/// `ok` reports whether the source answered; an unreadable source is not an empty one.
pub fn read<T>(op: &'static str, f: impl FnOnce() -> T, ok: impl FnOnce(&T) -> bool) -> T {
    let started = Instant::now();
    let value = f();
    let good = ok(&value);
    with(op, |c| {
        c.reads += 1;
        c.ms += started.elapsed().as_secs_f64() * 1000.0;
        if !good {
            c.failures += 1;
        }
    });
    value
}

/// Count an acquisition answered from a result this pass already holds.
pub fn shared(op: &'static str) {
    with(op, |c| c.shared += 1);
}

/// Start a pass: forget the previous one's counts on this thread and move its generation on,
/// which is what tells a shared observation from the previous pass apart from this one's.
pub fn reset() {
    COUNTS.with(|c| c.borrow_mut().clear());
    PASS.with(|p| p.set(p.get().wrapping_add(1)));
}

/// This thread's current pass. A cache that shares one acquisition across a refresh holds this
/// number beside its value and acquires again when the number moves.
pub fn pass() -> u64 {
    PASS.with(Cell::get)
}

/// This thread's counts since `reset`.
pub fn snapshot() -> BTreeMap<&'static str, Counts> {
    COUNTS.with(|c| c.borrow().clone())
}

/// Subprocesses this thread launched since `reset`, across every category.
pub fn spawns() -> u64 {
    COUNTS.with(|c| c.borrow().values().map(|v| v.spawns).sum())
}

/// Subprocesses this thread launched in one category since `reset`.
pub fn spawns_of(op: &'static str) -> u64 {
    COUNTS.with(|c| c.borrow().get(op).map_or(0, |v| v.spawns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_counts_its_own_spawns_and_keeps_other_threads_out() {
        reset();
        assert_eq!(spawns(), 0);
        let out = spawn(op::LIVENESS, &mut Command::new("/usr/bin/true")).unwrap();
        assert!(out.status.success());
        let _ = spawn(op::GIT, &mut Command::new("/usr/bin/false"));
        let mine = snapshot();
        assert_eq!(mine[op::LIVENESS].spawns, 1);
        assert_eq!(mine[op::LIVENESS].failures, 0);
        assert_eq!(mine[op::GIT].spawns, 1, "a failed command still spawned");
        assert_eq!(mine[op::GIT].failures, 1);
        assert_eq!(spawns(), 2);
        assert_eq!(spawns_of(op::GIT), 1);
        assert_eq!(
            std::thread::spawn(spawns).join().unwrap(),
            0,
            "another thread is another pass"
        );
        let generation = pass();
        reset();
        assert_eq!(snapshot(), BTreeMap::new());
        assert_ne!(pass(), generation, "a new pass is a new generation");
    }

    #[test]
    fn an_unreadable_source_counts_as_a_failed_read_and_a_reuse_is_not_a_read() {
        reset();
        assert_eq!(read(op::SQLITE, || Some(3), Option::is_some), Some(3));
        assert_eq!(read(op::SQLITE, || None::<i32>, Option::is_some), None);
        shared(op::PROCESS_TABLE);
        let mine = snapshot();
        assert_eq!((mine[op::SQLITE].reads, mine[op::SQLITE].failures), (2, 1));
        assert_eq!(mine[op::SQLITE].spawns, 0, "a query is not a subprocess");
        assert_eq!(
            (
                mine[op::PROCESS_TABLE].shared,
                mine[op::PROCESS_TABLE].reads
            ),
            (1, 0)
        );
        reset();
    }
}
