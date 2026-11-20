//! Places in the session list, driven through the real dashboard and `cones place`.
//!
//! The `claude` on the launch path is `tests/fixtures/fake_claude_fleet.py`, as in
//! `dashboard_claude.rs`, whose seeding helpers these flows share. A session sits in any number
//! of places: its folder, a `# tag` group above the folders, a `# tag` heading inside its folder,
//! and `pinned`. Each place is one row; the copies agree on state and stop together, the cursor
//! keeps the copy it was on, and each copy counts the others. ctrl+l is a checklist, a mouse drag
//! moves one copy, a drop on another folder is refused, removing every place restores the folder,
//! and ctrl+b reorders a group by hand, which keeps that group's order from then on. Pins and the
//! one-tag file from before places migrate, a conversation continued under a new id keeps its
//! places, a launch from a group sits in that group alone, and `cones place` writes the same
//! file. Each flow keeps `state/tags.json`.
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

/// The list side of every screen line.
fn list(screen: &str) -> impl Iterator<Item = &str> {
    screen.lines().map(|l| l.split('│').next().unwrap())
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
                && !l.starts_with("┆")
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

/// Press the left button on `from`, drag it to `to` and release it there.
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
    d.wait_text("places · space checks")
}

#[test]
fn a_session_sits_in_one_two_or_five_places_and_its_copies_act_as_one() {
    let mut d = Dashboard::new("places-many", &["claude"]);
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
    // A pin kept before places: its file becomes `pinned` on the first read.
    fs::write(
        d.path("state/pins.json"),
        serde_json::to_vec(&[&pin.id]).unwrap(),
    )
    .unwrap();
    for place in ["# sales", "folder › # legal", "pinned", "# urgent"] {
        cli(&d, &["place", &five.id, "add", place]);
    }
    cli(&d, &["place", &two.id, "add", "# sales"]);
    cli(&d, &["place", &two.id, "add", "folder > # legal"]);
    assert_eq!(
        places(&d, &five.id),
        json!([
            "folder",
            "pinned",
            "# sales",
            "# urgent",
            "folder › # legal"
        ])
    );
    assert_eq!(places(&d, &pin.id), json!(["pinned"]));
    assert_eq!(places(&d, &one.id), Value::Null);
    d.start();
    let screen = d.wait_for("every place", |s| sub(s, "# urgent").is_some());
    d.capture("many");

    // One row per place, each under its heading, each counting the others.
    let fives = rows_of(&screen, "Answer sales");
    let mut groups: Vec<String> = fives.iter().map(|&l| under(&screen, l)).collect();
    groups.sort();
    assert_eq!(
        groups,
        ["# legal", "# sales", "# urgent", p.as_str(), "pinned"],
        "{screen}"
    );
    for &l in &fives {
        let line = list(&screen).nth(l).unwrap();
        assert!(line.contains("Answer sales +4"), "{line}");
        assert!(
            line.contains("idle"),
            "every copy reports one state: {line}"
        );
    }
    let twos = rows_of(&screen, "Build export");
    let mut groups: Vec<String> = twos.iter().map(|&l| under(&screen, l)).collect();
    groups.sort();
    assert_eq!(groups, ["# legal", "# sales", o.as_str()], "{screen}");
    assert_eq!(rows_of(&screen, "Draft notes").len(), 1, "{screen}");
    assert!(!line_of(&screen, "Draft notes").contains('+'), "{screen}");
    assert_eq!(rows_of(&screen, "Clean the data").len(), 1, "{screen}");
    assert_eq!(under(&screen, row(&screen, "Clean the data")), "pinned");
    // A row under a tag above the folders names its folder; one inside a folder does not.
    for &l in &fives {
        let line = list(&screen).nth(l).unwrap();
        let group = under(&screen, l);
        let above = group == "pinned"
            || (group.starts_with('#')
                && !list(&screen)
                    .nth(subs(&screen, &group)[0])
                    .unwrap()
                    .starts_with("  "));
        assert_eq!(line.contains("· in project"), above, "{group}: {line}");
    }
    assert!(
        screen.contains("1 input"),
        "the header counts each session once:\n{screen}"
    );
    assert!(screen.contains("3 idle"), "{screen}");
    assert_eq!(places(&d, &pin.id), json!(["pinned"]));

    // The cursor walks the copies as rows, and the others carry a faint mark.
    select_in(&mut d, "Answer sales", "# sales");
    let screen = d.capture("copy-selected");
    assert_eq!(
        list(&screen).filter(|l| l.starts_with("┆ ")).count(),
        4,
        "{screen}"
    );
    // A change keeps the cursor on the copy it was on.
    d.press("ctrl+t", CTRL_T);
    let screen = d.wait_for("unpinned", |s| rows_of(s, "Answer sales").len() == 4);
    let at = rows_of(&screen, "▌ ")[0];
    assert!(
        list(&screen).nth(at).unwrap().contains("Answer sales"),
        "{screen}"
    );
    assert_eq!(under(&screen, at), "# sales", "{screen}");

    // A drag moves only the copy dragged: out of `# sales`, into `# urgent`.
    let screen = d.screen();
    let from = rows_of(&screen, "Build export")[0];
    assert_eq!(under(&screen, from), "# sales");
    drag(&mut d, "Build export", "# urgent");
    let screen = d.wait_for("the moved copy", |s| {
        rows_of(s, "Build export")
            .iter()
            .any(|&l| under(s, l) == "# urgent")
    });
    d.capture("moved");
    let mut groups: Vec<String> = rows_of(&screen, "Build export")
        .iter()
        .map(|&l| under(&screen, l))
        .collect();
    groups.sort();
    assert_eq!(groups, ["# legal", "# urgent", o.as_str()], "{screen}");
    assert_eq!(
        places(&d, &two.id),
        json!(["folder", "# urgent", "folder › # legal"])
    );

    // Another folder's heading, or a tag inside another folder, refuses a drop.
    drag(&mut d, "Clean the data", &o);
    d.wait_text("A session keeps its folder.");
    let legal = subs(&d.screen(), "# legal");
    assert!(
        under(&d.screen(), legal[0]).ends_with("other"),
        "{}",
        d.screen()
    );
    drag(&mut d, "Clean the data", "# legal");
    let screen = d.wait_text("A session keeps its folder.");
    d.capture("refused");
    assert_eq!(places(&d, &pin.id), json!(["pinned"]), "{screen}");

    // A name typed on `+ new tag…` and one Enter add the tag and keep the checklist. With the
    // folder still checked, the tag sits inside the folder and the session is listed once.
    select(&mut d, "Draft notes");
    let screen = checklist(&mut d);
    d.capture("checklist");
    assert!(chosen(&screen).ends_with(&format!("[x] {p}")), "{screen}");
    assert!(screen.contains("[ ] # legal"), "{screen}");
    assert!(!screen.contains("› # legal"), "one line per tag:\n{screen}");
    assert!(screen.contains("[ ] # urgent"), "{screen}");
    d.press("up", UP);
    d.typed("follow up");
    d.press("enter", b"\r");
    let screen = d.wait_for("the moved row", |s| {
        subs(s, "# follow up").len() == 1 && rows_of(s, "Draft notes").len() == 1
    });
    assert_eq!(
        under(&screen, row(&screen, "Draft notes")),
        "# follow up",
        "{screen}"
    );
    assert_eq!(places(&d, &one.id), json!(["folder › # follow up"]));

    // Unchecking every place restores the folder.
    select_in(&mut d, "Answer sales", "# sales");
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
    let screen = d.wait_for("one row again", |s| rows_of(s, "Answer sales").len() == 1);
    d.capture("folder-again");
    assert_eq!(under(&screen, row(&screen, "Answer sales")), p);
    assert!(!line_of(&screen, "Answer sales").contains('+'), "{screen}");
    assert_eq!(places(&d, &five.id), Value::Null);
    assert!(
        sub(&screen, "# sales").is_none(),
        "an empty tag leaves:\n{screen}"
    );

    // Stopping one copy stops the session: every copy leaves.
    select(&mut d, "Build export");
    d.press("ctrl+x", CTRL_X);
    d.wait_text("ctrl+x again");
    d.press("ctrl+x", CTRL_X);
    let screen = d.wait_for("every copy gone", |s| !s.contains("Build export"));
    d.capture("stopped");
    assert!(sub(&screen, "# urgent").is_none(), "{screen}");
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
    // A conversation continued under a new id lists the places of the one it came from.
    assert_eq!(cli(&d, &["place", &resumed.id]), "# analysis\n");
    let out = d.cli(&["place", &study.id, "add", "nowhere"], &[]);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(
        cli(&d, &["place", &study.id, "add", "# review"]),
        "# review\nfolder › # study\n"
    );
    assert_eq!(
        cli(&d, &["place", &study.id, "remove", "folder › # study"]),
        "# review\n"
    );
    assert_eq!(
        cli(&d, &["place", &study.id, "remove", "# review"]),
        "folder\n"
    );
    assert_eq!(places(&d, &study.id), Value::Null);
    cli(&d, &["place", &study.id, "add", "folder › # study"]);
    cli(&d, &["place", &study.id, "remove", "folder"]);
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
        "the continued conversation keeps its places:\n{screen}"
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
