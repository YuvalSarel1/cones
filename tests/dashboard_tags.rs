//! Places in the session list, driven through the real dashboard and `cones place`.
//!
//! The `claude` on the launch path is `tests/fixtures/fake_claude_fleet.py`, as in
//! `dashboard_claude.rs`, whose seeding helpers these flows share. A session sits in exactly one
//! place: its folder, a `# tag` group above the folders, a `# tag` heading inside its folder, or
//! `pinned`, and is listed once. Every change moves it: `cones place set`, ctrl+t, ctrl+l, whose
//! checklist keeps one tag checked, and a mouse drag; a drop on another folder is refused, and
//! unchecking everything restores the folder. ctrl+b reorders a group by hand, which keeps that
//! group's order from then on; a row listed beneath another moves only among its siblings. Pins,
//! the one-tag file and the several-place file from earlier cones migrate, a conversation
//! continued under a new id keeps its place, a launch from a group sits in that group, and
//! `cones place` writes the same file. Each flow keeps `state/tags.json`.
use crate::dashboard::*;
use crate::dashboard_claude::{
    Seeded, heading, heading_name, line_of, real_project, row, seed, selected,
};
use serde_json::{Value, json};
use std::{fs, path::Path};

const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const CTRL_B: &[u8] = b"\x02";
const CTRL_L: &[u8] = b"\x0c";
const CTRL_T: &[u8] = b"\x14";
const CTRL_X: &[u8] = b"\x18";

/// The list side of every screen line: up to the pane's border, which the rule above the
/// composer meets. A tree line inside the list is drawn with the same `│`.
fn list(screen: &str) -> impl Iterator<Item = &str> {
    let border = screen
        .lines()
        .find(|l| l.starts_with('─') && l.contains('│'))
        .and_then(|l| l.chars().position(|c| c == '│'));
    screen.lines().map(move |l| match border {
        Some(at) => l.char_indices().nth(at).map_or(l, |(i, _)| &l[..i]),
        None => l,
    })
}

/// The heading line `name` names, indented or not.
fn sub(screen: &str, name: &str) -> Option<usize> {
    screen.lines().position(|l| heading_name(l) == name)
}

/// Every heading `name` names, indented or not.
fn subs(screen: &str, name: &str) -> Vec<usize> {
    screen
        .lines()
        .enumerate()
        .filter(|(_, l)| heading_name(l) == name)
        .map(|(i, _)| i)
        .collect()
}

/// The list lines holding `text`.
fn rows_of(screen: &str, text: &str) -> Vec<usize> {
    list(screen)
        .enumerate()
        .filter(|(_, l)| l.contains(text))
        .map(|(i, _)| i)
        .collect()
}

/// The heading a list line sits under: the nearest heading line above it.
fn under(screen: &str, line: usize) -> String {
    let lines: Vec<&str> = list(screen).collect();
    lines[..line]
        .iter()
        .rev()
        .find(|l| {
            let t = heading_name(l);
            !l.is_empty()
                && (t.starts_with('#') || t == "pinned" || t.starts_with('/'))
                && !l.starts_with("▌")
        })
        .map(|l| heading_name(l).to_owned())
        .unwrap_or_default()
}

fn stored(d: &Dashboard) -> Value {
    serde_json::from_str(&fs::read_to_string(d.path("state/tags.json")).unwrap_or_default())
        .unwrap_or(Value::Null)
}

fn places(d: &Dashboard, id: &str) -> Value {
    stored(d)["places"].get(id).cloned().unwrap_or(Value::Null)
}

fn cli(d: &Dashboard, args: &[&str]) -> String {
    let out = d.cli(args, &[]);
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Press an arrow and wait for the screen to change; `false` at the list's edge.
fn step(d: &mut Dashboard, key: &[u8]) -> bool {
    let before = d.screen();
    d.press(if key == DOWN { "down" } else { "up" }, key);
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(2) {
        if d.screen() != before {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// Move the selection onto the first row titled `title` in the walk down, then up.
fn select(d: &mut Dashboard, title: &str) {
    for key in [DOWN, UP] {
        for _ in 0..30 {
            if selected(&d.screen()).contains(title) {
                return;
            }
            if !step(d, key) {
                break;
            }
        }
    }
    assert!(selected(&d.screen()).contains(title), "{}", d.screen());
}

/// Move the selection onto the row titled `title` under the heading `group`.
fn select_in(d: &mut Dashboard, title: &str, group: &str) {
    let on = |s: &str| {
        let line = rows_of(s, "▌ ").first().copied();
        line.is_some_and(|l| list(s).nth(l).unwrap().contains(title) && under(s, l) == group)
    };
    for key in [UP, DOWN] {
        for _ in 0..40 {
            if on(&d.screen()) {
                return;
            }
            if !step(d, key) {
                break;
            }
        }
    }
    assert!(on(&d.screen()), "{title} under {group}:\n{}", d.screen());
}

/// Where `text` starts on the screen, as the 1-based cell a mouse report names: the heading
/// it names if there is one, else its first appearance.
fn cell(screen: &str, text: &str) -> (usize, usize) {
    let at = |(r, l): (usize, &str)| l.find(text).map(|b| (l[..b].chars().count() + 1, r + 1));
    sub(screen, text)
        .and_then(|r| at((r, screen.lines().nth(r).unwrap())))
        .or_else(|| screen.lines().enumerate().find_map(at))
        .unwrap_or_else(|| panic!("{text:?} is not on screen:\n{screen}"))
}

/// Press the left button on `from`, drag it to `to` and release it there. While it moves,
/// the row it would drop on is underlined and the dragged row keeps its look.
fn drag(d: &mut Dashboard, from: &str, to: &str) {
    let screen = d.screen();
    let (fx, fy) = cell(&screen, from);
    let (tx, ty) = cell(&screen, to);
    d.press(
        &format!("press on {from:?}"),
        format!("\x1b[<0;{fx};{fy}M").as_bytes(),
    );
    d.press(
        &format!("drag to {to:?}"),
        format!("\x1b[<32;{tx};{ty}M").as_bytes(),
    );
    d.wait_for("the drop row underlined", |_| {
        let style = |x, y| d.cell_at(x, y).map(|c| (c.inverse(), c.underline()));
        style(fx, fy) == Some((false, false)) && style(tx, ty) == Some((false, true))
    });
    d.press("release", format!("\x1b[<0;{tx};{ty}m").as_bytes());
}

/// The checklist line the cursor is on.
fn chosen(screen: &str) -> String {
    list(screen)
        .find(|l| l.starts_with("› "))
        .unwrap_or_else(|| panic!("no checklist:\n{screen}"))
        .trim_end()
        .to_owned()
}

/// Open the checklist for the selected row.
fn checklist(d: &mut Dashboard) -> String {
    d.press("ctrl+l", CTRL_L);
    d.wait_text("place · space checks")
}

/// Move the checklist cursor onto the line containing `text`.
fn go(d: &mut Dashboard, text: &str) {
    for key in [DOWN, UP] {
        for _ in 0..20 {
            if chosen(&d.screen()).contains(text) {
                return;
            }
            if !step(d, key) {
                break;
            }
        }
    }
    assert!(
        chosen(&d.screen()).contains(text),
        "{text}:\n{}",
        d.screen()
    );
}

/// Every session row on the screen, each at most once, and no row naming another listing.
fn listed_once(screen: &str, titles: &[&str]) {
    for t in titles {
        assert!(rows_of(screen, t).len() <= 1, "{t} listed twice:\n{screen}");
        if let Some(&l) = rows_of(screen, t).first() {
            let line = list(screen).nth(l).unwrap();
            assert!(!line.contains(&format!("{t} +")), "{line}");
        }
    }
    assert!(!list(screen).any(|l| l.starts_with("┆")), "{screen}");
}

#[test]
fn a_session_sits_in_exactly_one_place_and_every_change_moves_it() {
    let mut d = Dashboard::new("places-one", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    fs::create_dir_all(d.path("other")).unwrap();
    let other = d.path("other").canonicalize().unwrap();
    d.open_folders(&[project.clone(), other.clone()]);
    let (p, o) = (project.display().to_string(), other.display().to_string());
    let five = seed(&d, &project, 1, "Answer sales", "idle", None);
    let two = seed(&d, &other, 2, "Build export", "idle", None);
    let pin = seed(&d, &project, 3, "Clean the data", "waiting", None);
    let one = seed(&d, &project, 4, "Draft notes", "idle", None);
    let titles = [
        "Answer sales",
        "Build export",
        "Clean the data",
        "Draft notes",
    ];
    // A pin kept before places: its file becomes `pinned` on the first read.
    fs::write(
        d.path("state/pins.json"),
        serde_json::to_vec(&[&pin.id]).unwrap(),
    )
    .unwrap();
    assert_eq!(cli(&d, &["place", &pin.id]), "pinned\n");
    // Sessions an earlier cones listed in several places keep one: a tag over the folder, of
    // several tags the last stored, and the file is written back with one each.
    let mut tags = stored(&d);
    tags["places"][&five.id] = json!([
        "folder",
        "pinned",
        "# sales",
        "# urgent",
        "folder › # legal"
    ]);
    tags["places"][&two.id] = json!(["folder", "# sales"]);
    fs::write(d.path("state/tags.json"), tags.to_string()).unwrap();
    assert_eq!(cli(&d, &["place", &five.id]), "folder › # legal\n");
    assert_eq!(places(&d, &five.id), json!(["folder › # legal"]));
    assert_eq!(places(&d, &two.id), json!(["# sales"]));
    // `set` moves the session out of where it was; `add` is the older spelling of it.
    assert_eq!(
        cli(&d, &["place", &five.id, "add", "# urgent"]),
        "# urgent\n"
    );
    assert_eq!(cli(&d, &["place", &five.id, "set", "# sales"]), "# sales\n");
    assert_eq!(places(&d, &five.id), json!(["# sales"]));
    assert_eq!(places(&d, &pin.id), json!(["pinned"]));
    assert_eq!(places(&d, &one.id), Value::Null);
    d.start();
    let screen = d.wait_for("every place", |s| sub(s, "# sales").is_some());
    d.capture("one-place");

    // One row per session, in its one place.
    listed_once(&screen, &titles);
    assert_eq!(
        under(&screen, row(&screen, "Answer sales")),
        "# sales",
        "{screen}"
    );
    assert_eq!(
        under(&screen, row(&screen, "Build export")),
        "# sales",
        "{screen}"
    );
    assert_eq!(under(&screen, row(&screen, "Clean the data")), "pinned");
    assert_eq!(under(&screen, row(&screen, "Draft notes")), p);
    assert!(sub(&screen, "# urgent").is_none() && sub(&screen, "# legal").is_none());
    // A row under a tag above the folders names its folder; one inside a folder does not.
    assert!(
        line_of(&screen, "Answer sales").contains("· in project"),
        "{screen}"
    );
    assert!(
        !line_of(&screen, "Draft notes").contains("· in "),
        "{screen}"
    );
    assert!(screen.contains("1 input"), "{screen}");
    assert!(screen.contains("3 idle"), "{screen}");

    // ctrl+t moves the session under `pinned`, and again back to its folder.
    select(&mut d, "Draft notes");
    d.press("ctrl+t", CTRL_T);
    let screen = d.wait_for("pinned", |s| under(s, row(s, "Draft notes")) == "pinned");
    listed_once(&screen, &titles);
    assert_eq!(places(&d, &one.id), json!(["pinned"]));
    d.press("ctrl+t", CTRL_T);
    let screen = d.wait_for("unpinned", |s| under(s, row(s, "Draft notes")) == p);
    listed_once(&screen, &titles);
    assert_eq!(places(&d, &one.id), Value::Null);

    // A drag moves the session: out of `# sales`, under `pinned`.
    drag(&mut d, "Build export", "pinned");
    let screen = d.wait_for("the moved row", |s| {
        under(s, row(s, "Build export")) == "pinned"
    });
    d.capture("moved");
    listed_once(&screen, &titles);
    assert_eq!(places(&d, &two.id), json!(["pinned"]));

    // Another folder's heading refuses a drop.
    drag(&mut d, "Clean the data", &o);
    let screen = d.wait_text("A session keeps its folder.");
    d.capture("refused");
    assert_eq!(places(&d, &pin.id), json!(["pinned"]), "{screen}");

    // The checklist keeps one tag checked: checking `pinned` unchecks `# sales`, the folder
    // does not go with `pinned`, and the folder with a tag nests it.
    select(&mut d, "Answer sales");
    let screen = checklist(&mut d);
    d.capture("checklist");
    assert!(screen.contains(&format!("[ ] {p}")), "{screen}");
    assert!(screen.contains("[x] # sales"), "{screen}");
    go(&mut d, "pinned");
    d.press("space", b" ");
    let screen = d.wait_text("[x] pinned");
    assert!(screen.contains("[ ] # sales"), "{screen}");
    go(&mut d, &p);
    d.press("space", b" ");
    let screen = d.wait_text(&format!("[x] {p}"));
    assert!(screen.contains("[ ] pinned"), "{screen}");
    // A new tag typed on its row and one Enter keep the checklist with that tag in place of
    // any other, nested in the checked folder.
    go(&mut d, "+ new tag");
    d.typed("follow up");
    d.press("enter", b"\r");
    let screen = d.wait_for("the moved row", |s| {
        sub(s, "# follow up").is_some() && sub(s, "# sales").is_none()
    });
    listed_once(&screen, &titles);
    assert_eq!(
        under(&screen, row(&screen, "Answer sales")),
        "# follow up",
        "{screen}"
    );
    assert_eq!(places(&d, &five.id), json!(["folder › # follow up"]));

    // Unchecking everything restores the folder.
    checklist(&mut d);
    for _ in 0..20 {
        let line = chosen(&d.screen());
        if line.contains("+ new tag") {
            break;
        }
        if line.contains("[x]") {
            d.press("space", b" ");
        }
        d.press("down", DOWN);
    }
    let screen = d.screen();
    assert!(!screen.contains("[x]"), "{screen}");
    d.press("enter", b"\r");
    let screen = d.wait_for("its folder again", |s| sub(s, "# follow up").is_none());
    d.capture("folder-again");
    listed_once(&screen, &titles);
    assert_eq!(under(&screen, row(&screen, "Answer sales")), p);
    assert_eq!(places(&d, &five.id), Value::Null);

    // Stopping the session takes its one row away.
    select(&mut d, "Build export");
    d.press("ctrl+x", CTRL_X);
    d.wait_text("ctrl+x again");
    d.press("ctrl+x", CTRL_X);
    d.wait_for("the row gone", |s| !s.contains("Build export"));
    d.capture("stopped");
    d.keep("state/tags.json");
    d.quit();
}

/// Delete on a tag's checklist line marks it, a second press unmarks it, and Enter takes the
/// tag off every session, each returning to its folder; a session in another place keeps it.
#[test]
fn delete_in_the_checklist_removes_a_tag_from_every_session() {
    let mut d = Dashboard::new("places-remove-tag", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    d.open_folders(std::slice::from_ref(&project));
    let p = project.display().to_string();
    let nested = seed(&d, &project, 1, "Answer sales", "idle", None);
    let pinned = seed(&d, &project, 2, "Build export", "idle", None);
    let only = seed(&d, &project, 3, "Draft notes", "idle", None);
    cli(&d, &["place", &nested.id, "set", "folder › # legal"]);
    cli(&d, &["place", &pinned.id, "set", "pinned"]);
    cli(&d, &["place", &only.id, "set", "# legal"]);
    d.start();
    d.wait_for("the legal tags", |s| subs(s, "# legal").len() == 2);
    select_in(&mut d, "Draft notes", "# legal");

    checklist(&mut d);
    d.press("down", DOWN);
    let line = chosen(&d.screen());
    assert!(
        line.contains("[x] # legal · del removes it everywhere"),
        "{line}"
    );
    d.press("delete", b"\x1b[3~");
    d.wait_text("removed from every session");
    d.press("backspace", b"\x7f");
    d.wait_for("unmarked", |s| !s.contains("removed from every session"));
    d.press("delete", b"\x1b[3~");
    d.wait_text("removed from every session");
    d.capture("marked");
    d.press("enter", b"\r");

    let screen = d.wait_for("the tag gone", |s| subs(s, "# legal").is_empty());
    d.capture("removed");
    assert!(screen.contains("removed # legal"), "{screen}");
    assert_eq!(under(&screen, row(&screen, "Answer sales")), p);
    assert_eq!(under(&screen, row(&screen, "Draft notes")), p);
    assert_eq!(under(&screen, row(&screen, "Build export")), "pinned");
    assert_eq!(places(&d, &nested.id), json!(["folder"]));
    assert_eq!(places(&d, &pinned.id), json!(["pinned"]));
    assert_eq!(places(&d, &only.id), Value::Null);
    d.keep("state/tags.json");
    d.quit();
}

#[test]
fn reordering_by_hand_keeps_the_group_s_order_and_says_so_once() {
    let mut d = Dashboard::new("places-reorder", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let p = project.display().to_string();
    let first = seed(&d, &project, 1, "First started", "idle", None);
    let second = seed(&d, &project, 2, "Second started", "idle", None);
    let third = seed(&d, &project, 3, "Third started", "idle", None);
    d.start();
    let screen = d.wait_text("Third started");
    let order = |s: &str| {
        let mut rows = [
            (row(s, "First started"), "first"),
            (row(s, "Second started"), "second"),
            (row(s, "Third started"), "third"),
        ];
        rows.sort();
        rows.map(|(_, name)| name)
    };
    assert_eq!(order(&screen), ["first", "second", "third"], "{screen}");

    // ctrl+b picks the row up, the arrows move it, enter keeps it.
    select(&mut d, "Third started");
    d.press("ctrl+b", CTRL_B);
    d.wait_text("moving inside");
    d.press("up", UP);
    d.press("up", UP);
    let screen = d.wait_for("the row moved", |s| order(s)[0] == "third");
    // The picked-up row is in reverse video while it moves.
    let (x, y) = cell(&screen, "Third started");
    d.wait_for("the moving row highlighted", |_| {
        d.cell_at(x, y).is_some_and(|c| c.inverse())
    });
    d.capture("moving");
    d.press("enter", b"\r");
    // The footer is as wide as the list; this fixture's folder path fills most of it.
    let screen2 = d.wait_text("now sorts by hand");
    d.capture("kept");
    assert!(
        screen2.contains(&format!("{p} now sorts by hand (")),
        "{screen2}"
    );
    assert_eq!(order(&screen), ["third", "first", "second"]);
    assert_eq!(
        stored(&d)["order"][&p],
        json!([&third.id, &first.id, &second.id])
    );

    // A mouse drag inside the group moves a row there too, and says nothing more.
    drag(&mut d, "Second started", "Third started");
    let screen = d.wait_for("the dragged row", |s| order(s)[0] == "second");
    d.capture("dragged");
    assert!(!screen.contains("now sorts by hand"), "{screen}");
    assert_eq!(
        stored(&d)["order"][&p],
        json!([&second.id, &third.id, &first.id])
    );

    // esc puts a picked-up row back.
    select(&mut d, "First started");
    d.press("ctrl+b", CTRL_B);
    d.wait_text("moving inside");
    d.press("up", UP);
    d.wait_for("the row moved", |s| order(s)[1] == "first");
    d.press("esc", b"\x1b");
    let screen = d.wait_for("the row back", |s| order(s)[2] == "first");
    assert_eq!(order(&screen), ["second", "third", "first"]);
    d.keep("state/tags.json");
    d.quit();
}

#[test]
fn a_row_listed_beneath_another_moves_among_its_siblings() {
    let mut d = Dashboard::new("places-reorder-beneath", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let p = project.display().to_string();
    let lead = seed(&d, &project, 1, "Lead the work", "idle", None);
    let one = seed(&d, &project, 2, "Worker one", "idle", None);
    let two = seed(&d, &project, 3, "Worker two", "idle", None);
    let other = seed(&d, &project, 4, "Other top row", "idle", None);
    cli(&d, &["adopt", &one.id, "--parent", &lead.id]);
    cli(&d, &["adopt", &two.id, "--parent", &lead.id]);
    d.start();
    let screen = d.wait_text("Other top row");
    let order = |s: &str| {
        let mut rows = [
            (row(s, "Lead the work"), "lead"),
            (row(s, "Worker one"), "one"),
            (row(s, "Worker two"), "two"),
            (row(s, "Other top row"), "other"),
        ];
        rows.sort();
        rows.map(|(_, name)| name)
    };
    assert_eq!(order(&screen), ["lead", "one", "two", "other"], "{screen}");

    // ctrl+b picks up a launched row; it moves past its sibling and stays beneath its launcher.
    select(&mut d, "Worker two");
    d.press("ctrl+b", CTRL_B);
    d.wait_text("moving inside");
    d.press("up", UP);
    d.press("up", UP);
    d.wait_for("the row moved", |s| order(s)[1] == "two");
    d.press("enter", b"\r");
    let screen = d.wait_text("now sorts by hand");
    assert_eq!(order(&screen), ["lead", "two", "one", "other"], "{screen}");
    assert_eq!(
        stored(&d)["order"][&p],
        json!([&lead.id, &other.id, &two.id, &one.id])
    );

    // Moving a top row keeps the siblings' order and carries them with their launcher.
    select(&mut d, "Other top row");
    d.press("ctrl+b", CTRL_B);
    d.wait_text("moving inside");
    d.press("up", UP);
    d.press("enter", b"\r");
    let screen = d.wait_for("the top row moved", |s| order(s)[0] == "other");
    assert_eq!(order(&screen), ["other", "lead", "two", "one"], "{screen}");

    // A launched row dropped on a top row is refused.
    drag(&mut d, "Worker one", "Other top row");
    let screen = d.wait_text("on its own level");
    assert_eq!(order(&screen), ["other", "lead", "two", "one"], "{screen}");
    d.keep("state/tags.json");
    d.quit();
}

/// Prepend the record Claude writes when it continues a conversation under a new id: the
/// copied records name the conversation it came from.
fn continued_from(d: &Dashboard, child: &Seeded, dir: &Path, parent: &str) {
    let key: String = dir
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let path = d
        .home()
        .join(".claude/projects")
        .join(key)
        .join(format!("{}.jsonl", child.id));
    let text = fs::read_to_string(&path).unwrap();
    let copied = json!({"session_id": parent, "type": "user", "cwd": dir,
        "message": {"role": "user", "content": "earlier"}});
    fs::write(&path, format!("{copied}\n{text}")).unwrap();
}

#[test]
fn tags_migrate_places_follow_a_continued_conversation_and_a_launch_sits_in_its_group() {
    let mut d = Dashboard::new("places-carry", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let p = project.display().to_string();
    let pinned = seed(&d, &project, 1, "Keep me on top", "idle", None);
    let study = seed(&d, &project, 2, "Plan the study", "idle", None);
    let cleared = seed(&d, &project, 3, "Cleared by hand", "idle", None);
    // A resumed conversation the harness names anew; the id it continues is not listed.
    let resumed = seed(&d, &project, 4, "Continue the analysis", "idle", None);
    let gone = uuid::Uuid::new_v4().to_string();
    continued_from(&d, &resumed, &project, &gone);
    // The one-tag file an earlier cones wrote: a tag on its own, one inside its folder, a pin,
    // a cleared tag, and a hand order.
    fs::write(
        d.path("state/tags.json"),
        json!({
            "tags": {&gone: "analysis", &study.id: "study", &pinned.id: "pinned",
                     &cleared.id: ""},
            "inside": [&study.id],
            "order": {&p: [&cleared.id, &pinned.id]},
        })
        .to_string(),
    )
    .unwrap();

    // `cones place` reads it migrated and writes the new form, losing nothing.
    assert_eq!(cli(&d, &["place", &study.id]), "folder › # study\n");
    assert_eq!(
        stored(&d),
        json!({
            "places": {&gone: ["# analysis"], &study.id: ["folder › # study"],
                       &pinned.id: ["pinned"], &cleared.id: ["folder"]},
            "order": {&p: [&cleared.id, &pinned.id]},
        })
    );
    // A conversation continued under a new id lists the place of the one it came from.
    assert_eq!(cli(&d, &["place", &resumed.id]), "# analysis\n");
    let out = d.cli(&["place", &study.id, "set", "nowhere"], &[]);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(
        cli(&d, &["place", &study.id, "set", "# review"]),
        "# review\n"
    );
    // `remove PLACE` names where the session is, or changes nothing.
    let out = d.cli(&["place", &study.id, "remove", "folder › # study"], &[]);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(places(&d, &study.id), json!(["# review"]));
    assert_eq!(
        cli(&d, &["place", &study.id, "remove", "# review"]),
        "folder\n"
    );
    assert_eq!(places(&d, &study.id), Value::Null);
    cli(&d, &["place", &study.id, "set", "# review"]);
    assert_eq!(cli(&d, &["place", &study.id, "remove"]), "folder\n");
    cli(&d, &["place", &study.id, "set", "folder › # study"]);
    let all = cli(&d, &["place"]);
    for line in [
        format!("{gone}\t# analysis\n"),
        format!("{}\tfolder › # study\n", study.id),
        format!("{}\tpinned\n", pinned.id),
    ] {
        assert!(all.contains(&line), "{all}");
    }

    // A ctrl+p mark on the conversation it came from marks the continued one too.
    let mut tags = stored(&d);
    tags["highlighted"] = json!([&gone]);
    fs::write(d.path("state/tags.json"), tags.to_string()).unwrap();

    d.start();
    let screen = d.wait_for("every place", |s| {
        heading(s, "pinned").is_some() && sub(s, "# analysis").is_some()
    });
    d.wait_for("the carried highlight", |_| {
        d.colour_of("Continue the analysis") == Some(vt100::Color::Idx(5))
    });
    assert_ne!(d.colour_of("Plan the study"), Some(vt100::Color::Idx(5)));
    select(&mut d, "Continue the analysis");
    d.press("ctrl+p", b"\x10");
    d.wait_for("the carried highlight cleared", |_| {
        d.colour_of("Continue the analysis") != Some(vt100::Color::Idx(5))
    });
    assert_eq!(stored(&d).get("highlighted"), None);
    d.capture("migrated-and-carried");
    let top = heading(&screen, "pinned").unwrap();
    assert_eq!(row(&screen, "Keep me on top"), top + 1, "{screen}");
    assert!(line_of(&screen, "Keep me on top").contains("Keep me on top · in project "));
    assert_eq!(
        under(&screen, row(&screen, "Continue the analysis")),
        "# analysis",
        "the continued conversation keeps its place:\n{screen}"
    );
    assert_eq!(under(&screen, row(&screen, "Plan the study")), "# study");
    assert!(
        list(&screen)
            .nth(sub(&screen, "# study").unwrap())
            .unwrap()
            .starts_with("  ")
    );
    assert_eq!(under(&screen, row(&screen, "Cleared by hand")), p);

    // A launch from a group runs in that row's folder and sits in that group alone.
    select(&mut d, "Plan the study");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");
    let screen = d.wait_for("the launch in its group", |s| {
        s.contains("Fixture title for the launch")
    });
    d.capture("launched-in-group");
    let rows = rows_of(&screen, "Fixture title for the launch");
    assert_eq!(rows.len(), 1, "{screen}");
    assert_eq!(under(&screen, rows[0]), "# study", "{screen}");
    let launched = crate::dashboard_claude::registry(&d)
        .into_iter()
        .filter_map(|e| e["sessionId"].as_str().map(str::to_owned))
        .find(|id| ![&pinned.id, &study.id, &resumed.id, &cleared.id].contains(&id))
        .expect("the launched session");
    d.wait_file("state/tags.json", |t| t.contains(&launched));
    assert_eq!(
        places(&d, &launched),
        json!(["folder › # study"]),
        "{screen}"
    );
    d.keep("state/tags.json");
    d.quit();
}
