//! The integration tests, linked into one binary. Each separate test binary links the whole
//! crate again and pays its own first-exec scan, so one binary builds and starts faster.
//! Select a file's tests by its module: `scripts/check test --test integration runner::`.
//! `cost.rs` is its own target: it owns the process-wide price service.

mod check_script;
mod codex;
// `core` would shadow the standard crate of that name.
#[path = "core.rs"]
mod core_cli;
mod history;
mod history_api;
mod launch;
mod opencode;
mod runner;
mod stop;
mod terminals;
mod transcript;
