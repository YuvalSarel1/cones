//! Tags in the session list, driven through the real dashboard and `cones tag`.
//!
//! The `claude` on the launch path is `tests/fixtures/fake_claude_fleet.py`, as in
//! `dashboard_claude.rs`, whose seeding helpers these flows share. A session sits under its tag
//! when it has one and under its folder otherwise: a tag in one folder is a heading inside it,
//! one across folders heads its own group and names each row's folder, and the last session
//! leaving a tag removes its heading. ctrl+l chooses the place, a mouse drag drops a row on a
//! heading or among its group, a drop on another folder is refused, and ctrl+b reorders a
//! group by hand, which keeps that group's order from then on. Pins kept before tags become the
//! `pinned` tag, a conversation continued under a new id keeps its tag, a launch from a tag's
//! group joins it, and `cones tag` writes the same file. Each flow keeps `state/tags.json`.
use crate::dashboard::*;
use crate::dashboard_claude::{Seeded, heading, line_of, real_project, row, seed, selected};
use serde_json::{Value, json};
use std::{fs, path::Path};

const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const CTRL_B: &[u8] = b"\x02";
const CTRL_L: &[u8] = b"\x0c";

/// The heading line `name` names, indented or not: the list side of the line.
fn sub(screen: &str, name: &str) -> Option<usize> {
    screen
        .lines()
        .position(|l| l.split('│').next().unwrap().trim() == name)
}

fn tags(d: &Dashboard) -> Value {
    serde_json::from_str(&fs::read_to_string(d.path("state/tags.json")).unwrap_or_default())
        .unwrap_or(Value::Null)
}

/// Move the selection onto the row titled `title`.
fn select(d: &mut Dashboard, title: &str) {
    for key in [DOWN, UP] {
        for _ in 0..20 {
            if selected(&d.screen()).contains(title) {
                return;
            }
            let before = d.screen();
            d.press(if key == DOWN { "down" } else { "up" }, key);
            if d.screen() == before {
                break;
            }
        }
    }
    assert!(selected(&d.screen()).contains(title), "{}", d.screen());
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

/// Choose the place picker's row `label` for the selected session.
fn place(d: &mut Dashboard, label: &str) {
    d.press("ctrl+l", CTRL_L);
    d.wait_text("place this session");
    for _ in 0..10 {
        if line_of(&d.screen(), "› ").contains(label) {
            break;
        }
        d.press("down", DOWN);
    }
    assert!(line_of(&d.screen(), "› ").contains(label), "{}", d.screen());
    d.press("enter", b"\r");
}

#[test]
fn a_tag_nests_in_its_folder_lifts_across_folders_and_leaves_with_its_last_session() {
    let mut d = Dashboard::new("tags-place", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    fs::create_dir_all(d.path("other")).unwrap();
    let other = d.path("other").canonicalize().unwrap();
    d.open_folders(&[project.clone(), other.clone()]);
    let (p, o) = (project.display().to_string(), other.display().to_string());
    let _train = seed(&d, &project, 1, "Train the model", "busy", None);
    let sales = seed(&d, &project, 2, "Sales churn by region", "idle", None);
    let cohort = seed(&d, &project, 3, "Cohort cut for Q3", "waiting", None);
    let export = seed(&d, &other, 4, "Export the churn csv", "idle", None);
    d.start();
    d.wait_text("Export the churn csv");

    // ctrl+l, then a new tag typed on the picker's last row.
    select(&mut d, "Sales churn by region");
    d.press("ctrl+l", CTRL_L);
    let screen = d.wait_text("place this session");
    d.capture("picker");
    let first = row(&screen, "place this session") + 1;
    assert!(
        screen
            .lines()
            .nth(first)
            .unwrap()
            .contains(&format!("● {p}")),
        "the session's own folder comes first and is where it sits:\n{screen}"
    );
    assert!(screen.contains("○ pinned · ctrl+t"), "{screen}");
    d.press("up", UP);
    d.typed("sales");
    d.wait_text("+ new tag… sales");
    d.press("enter", b"\r");
    let screen = d.wait_for("the nested tag", |s| sub(s, "# sales").is_some());
    d.capture("nested");
    let (folder, tag) = (
        heading(&screen, &p).unwrap(),
        sub(&screen, "# sales").unwrap(),
    );
    assert!(folder < tag, "{screen}");
    assert_eq!(row(&screen, "Sales churn by region"), tag + 1, "{screen}");
    assert!(
        !line_of(&screen, "Sales churn by region").contains(" · in "),
        "inside its folder a row names no folder:\n{screen}"
    );
    assert_eq!(tags(&d)["tags"], json!({&sales.id: "sales"}));

    // Dragged onto the tag's heading, a session joins it.
    drag(&mut d, "Cohort cut for Q3", "# sales");
    let screen = d.wait_for("the dropped session", |s| {
        sub(s, "# sales").is_some_and(|t| row(s, "Cohort cut for Q3") > t)
    });
    d.capture("dropped-on-tag");
    assert_eq!(tags(&d)["tags"][&cohort.id], "sales", "{screen}");

    // A session from another folder lifts the tag above the folders, each row naming its own.
    select(&mut d, "Export the churn csv");
    place(&mut d, "# sales");
    let screen = d.wait_for("the lifted tag", |s| heading(s, "# sales").is_some());
    d.capture("lifted");
    let tag = heading(&screen, "# sales").unwrap();
    assert!(tag < heading(&screen, &p).unwrap(), "{screen}");
    for (title, folder) in [
        ("Sales churn by region", "project"),
        ("Cohort cut for Q3", "project"),
        ("Export the churn csv", "other"),
    ] {
        let line = line_of(&screen, title);
        assert!(line.contains(&format!("{title} · in {folder} ")), "{line}");
        assert!(row(&screen, title) > tag, "{screen}");
    }
    assert!(
        heading(&screen, &o).is_some_and(
            |h| line_of(&screen, "no sessions here") == screen.lines().nth(h + 1).unwrap()
        ),
        "the open folder keeps its row with its only session tagged:\n{screen}"
    );

    // Dropped on its own folder's heading it leaves the tag, which nests again.
    drag(&mut d, "Export the churn csv", &o);
    let screen = d.wait_for("the tag back in its folder", |s| {
        sub(s, "# sales").is_some_and(|t| heading(s, "# sales").is_none() && t > 0)
    });
    d.capture("nested-again");
    assert!(row(&screen, "Export the churn csv") > heading(&screen, &o).unwrap());
    assert_eq!(tags(&d)["tags"].get(&export.id), None);

    // Another folder's heading refuses the drop, and the footer says why.
    drag(&mut d, "Cohort cut for Q3", &o);
    let screen = d.wait_text("A session keeps its folder.");
    d.capture("refused");
    // The note is cut at the list's edge, inside this fixture's long folder path.
    let shown = &format!("Drop it on a tag or on {p}.")[..30];
    assert!(screen.contains(shown), "{screen}");
    assert_eq!(tags(&d)["tags"][&cohort.id], "sales");

    // The picker's first row returns a session to its folder; the last one out removes the tag.
    select(&mut d, "Cohort cut for Q3");
    place(&mut d, &p);
    select(&mut d, "Sales churn by region");
    place(&mut d, &p);
    let screen = d.wait_for("no tag left", |s| sub(s, "# sales").is_none());
    d.capture("untagged");
    assert!(row(&screen, "Sales churn by region") > heading(&screen, &p).unwrap());
    assert_eq!(tags(&d)["tags"], json!({}));
    d.keep("state/tags.json");
    d.quit();
}

#[test]
fn reordering_by_hand_keeps_the_group_s_order_and_says_so_once() {
    let mut d = Dashboard::new("tags-reorder", &["claude"]);
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
        tags(&d)["order"][&p],
        json!([&third.id, &first.id, &second.id])
    );

    // A mouse drag inside the group moves a row there too, and says nothing more.
    drag(&mut d, "Second started", "Third started");
    let screen = d.wait_for("the dragged row", |s| order(s)[0] == "second");
    d.capture("dragged");
    assert!(!screen.contains("now sorts by hand"), "{screen}");
    assert_eq!(
        tags(&d)["order"][&p],
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
fn pins_migrate_tags_follow_a_continued_conversation_and_a_launch_joins_its_tag() {
    let mut d = Dashboard::new("tags-carry", &["claude"]);
    d.install("claude", "fake_claude_fleet.py");
    let project = real_project(&d);
    let p = project.display().to_string();
    let pinned = seed(&d, &project, 1, "Keep me on top", "idle", None);
    let study = seed(&d, &project, 2, "Plan the study", "idle", None);
    // A resumed conversation the harness names anew; the id it continues is not listed.
    let resumed = seed(&d, &project, 3, "Continue the analysis", "idle", None);
    let gone = uuid::Uuid::new_v4().to_string();
    continued_from(&d, &resumed, &project, &gone);
    fs::write(
        d.path("state/pins.json"),
        serde_json::to_vec(&[&pinned.id]).unwrap(),
    )
    .unwrap();

    // `cones tag` writes the file the dashboard reads; a conversation not listed takes one too.
    let out = d.cli(&["tag", &gone, "analysis"], &[]);
    assert!(out.status.success(), "{out:?}");
    let out = d.cli(&["tag", &study.id, "#study "], &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("tagged {} study\n", study.id)
    );
    let out = d.cli(&["tag"], &[]);
    let listed = String::from_utf8_lossy(&out.stdout).to_string();
    for (id, tag) in [
        (&gone, "analysis"),
        (&study.id, "study"),
        (&pinned.id, "pinned"),
    ] {
        assert!(listed.contains(&format!("{id}\t{tag}\n")), "{listed}");
    }
    let out = d.cli(&["tag", "--clear"], &[]);
    assert!(!out.status.success(), "--clear needs an id: {out:?}");
    let out = d.cli(&["tag", &study.id, "--clear"], &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(tags(&d)["tags"].get(&study.id), None);
    let out = d.cli(&["tag", &study.id, "study"], &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        tags(&d)["tags"],
        json!({&pinned.id: "pinned", &study.id: "study", &gone: "analysis"})
    );

    d.start();
    let screen = d.wait_for("every place", |s| {
        heading(s, "pinned").is_some() && sub(s, "# analysis").is_some()
    });
    d.capture("migrated-and-carried");
    // The pin kept before tags is the `pinned` tag, above the folders.
    let top = heading(&screen, "pinned").unwrap();
    assert_eq!(row(&screen, "Keep me on top"), top + 1, "{screen}");
    assert!(line_of(&screen, "Keep me on top").contains("Keep me on top · in project "));
    assert!(top < heading(&screen, &p).unwrap(), "{screen}");
    assert_eq!(
        row(&screen, "Continue the analysis"),
        sub(&screen, "# analysis").unwrap() + 1,
        "the continued conversation keeps its tag:\n{screen}"
    );
    assert!(
        fs::read_to_string(d.path("state/pins.json")).is_ok(),
        "pins.json stays"
    );

    // A launch from a tag's group runs in that row's folder and joins the tag.
    select(&mut d, "Plan the study");
    d.typed("count the flaky tests");
    d.press("enter", b"\r");
    let screen = d.wait_for("the launch under its tag", |s| {
        s.contains("Fixture title for the launch")
            && sub(s, "# study").is_some_and(|t| row(s, "Fixture title for the launch") > t)
    });
    d.capture("launched-in-tag");
    let launched = crate::dashboard_claude::registry(&d)
        .into_iter()
        .filter_map(|e| e["sessionId"].as_str().map(str::to_owned))
        .find(|id| ![&pinned.id, &study.id, &resumed.id].contains(&id))
        .expect("the launched session");
    d.wait_file("state/tags.json", |t| t.contains(&launched));
    assert_eq!(tags(&d)["tags"][&launched], "study", "{screen}");
    d.keep("state/tags.json");
    d.quit();
}
