//! The integration tests, linked into one binary. Each separate test binary links the whole
//! crate again and pays its own first-exec scan, so one binary builds and starts faster.
//! Select a file's tests by its module: `scripts/check test --test integration runner::`.
//! `cost.rs` is its own target: it owns the process-wide price service.

mod btw;
mod check_script;
mod codex;
// `core` would shadow the standard crate of that name.
#[path = "core.rs"]
mod core_cli;
mod dashboard;
mod dashboard_claude;
mod dashboard_config;
mod dashboard_grok;
mod dashboard_history;
mod dashboard_jobs;
mod dashboard_pane;
mod dashboard_pi;
mod dashboard_tags;
mod dashboard_terminal;
mod dashboard_update;
mod grok;
mod history;
mod history_api;
mod homes;
mod launch;
mod notes;
mod opencode;
mod runner;
mod show_screen;
mod stop;
mod terminals;
mod transcript;
