//! The config editor behind the dashboard's `config` button: the rows of `FIELDS` with the value
//! each one takes, and the keys that move between them. `ConfigForm` is what the editor has on
//! screen, `ConfigAction` is what a key asks the dashboard to do with it. The dashboard owns
//! saving, so the editor never writes `jobs.yaml` itself.
use super::*;

/// What a key in the config editor asks the dashboard to do.
#[derive(Debug, PartialEq)]
pub enum ConfigAction {
    Stay,
    Cancel,
    /// The `columns` row: the arranger on the table, which the dashboard opens where the
    /// table is rather than the editor drawing a second one over it.
    Columns,
    /// A field closed on a new value, so the block is written and the editor stays where it
    /// is: the `defaults` block, the `columns:` list, empty for the built-in, the `sparkline:`,
    /// `pane:` and `start:` blocks, None when every field of one is left to the built-in, and
    /// the `confirm_secs:` line.
    Save(
        Box<config::Policy>,
        Vec<String>,
        Option<config::Sparkline>,
        Option<config::Pane>,
        Option<config::Start>,
        Option<f64>,
    ),
}

/// The config editor the menu's `config` button opens: the `defaults` block of jobs.yaml, the
/// policy every job runs under unless it sets the field itself, and the dashboard's `columns:`
/// line and `sparkline:` block, one row per field under its group where the list is. Every row
/// is a label and the control the field takes, drawn whole: every word a pick offers with the
/// current one bracketed, a number between the arrows that step it, free text in a box. What a
/// field accepts is read off its row rather than found by opening it. `↑` `↓` move between
/// fields and `← →` change the selected one in place, writing the block under the key that
/// moved it; `backspace` puts the built-in back. There is nothing to press to keep the block,
/// and `esc` on the list only closes the editor. `enter` opens the one control a row cannot
/// draw whole, the text of a typed value, where `enter` keeps it and `esc` puts the old one
/// back. The selected field's fuller explanation sits under the list, and its key in jobs.yaml
/// on the prompt line. An empty answer leaves the field out of the file, so the built-in
/// applies and reads `default` in the control's place. Pure: the file is read and written by
/// the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigForm {
    pub row: usize,
    /// Each field as typed, in `FIELDS` order; a picked field holds its option's word, empty
    /// for the built-in.
    pub values: Vec<String>,
    pub error: Option<String>,
    /// The selected field is open for editing; `before` is its value when it was opened, put
    /// back by `esc`.
    pub open: bool,
    before: String,
    /// The cursor in the selected value, a byte offset; past the end means after it.
    cursor: usize,
    /// Only the `SESSION` rows are shown and visited, under one title and no group heads:
    /// the form `ctrl+o` opens, seeded from the policy the next session would run under.
    pub session: bool,
    /// The file's `columns:` line, carried through a save rather than edited here: the
    /// columns are arranged on the table itself, with `ctrl+t`.
    columns: Option<Vec<String>>,
}

impl ConfigForm {
    /// The `SESSION` rows alone, as the settings of the next session the composer starts.
    pub fn session(policy: &config::Policy) -> Self {
        let mut form = Self::new(policy, None, None, None, None, None);
        form.session = true;
        form
    }

    /// Whether row `i` is on screen.
    fn shown(&self, i: usize) -> bool {
        !self.session || SESSION.contains(&FIELDS[i].name)
    }

    pub fn new(
        d: &config::Policy,
        columns: Option<&[String]>,
        spark: Option<&config::Sparkline>,
        pane: Option<&config::Pane>,
        start: Option<&config::Start>,
        confirm_secs: Option<f64>,
    ) -> Self {
        let num = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
        let flag = |v: Option<bool>| v.map(|v| v.to_string()).unwrap_or_default();
        let spark = |f: fn(&config::Sparkline) -> String| spark.map(f).unwrap_or_default();
        let pane = |f: fn(&config::Pane) -> String| pane.map(f).unwrap_or_default();
        let values = FIELDS
            .iter()
            .map(|f| match f.name {
                "timeout_min" => num(d.timeout_min),
                "budget_usd" => num(d.budget_usd),
                "daily_budget_usd" => num(d.daily_budget_usd),
                "write" => flag(d.write),
                "overlap" => d
                    .overlap
                    .map(|o| match o {
                        config::Overlap::Skip => "skip",
                        config::Overlap::Allow => "allow",
                        config::Overlap::Replace => "replace",
                    })
                    .unwrap_or_default()
                    .to_owned(),
                "model" => d.model.clone().unwrap_or_default(),
                "harness" => d.harness.map(|h| h.to_string()).unwrap_or_default(),
                "max_turns" => d.max_turns.map(|v| v.to_string()).unwrap_or_default(),
                "codex_model" => d.codex_model.clone().unwrap_or_default(),
                "codex_full_access" => flag(d.codex_full_access),
                "notify" => flag(d.notify),
                "bedrock" => flag(d.bedrock),
                "aws_profile" => d.aws_profile.clone().unwrap_or_default(),
                "aws_region" => d.aws_region.clone().unwrap_or_default(),
                "start.harness" => start.map(|s| s.harness.to_string()).unwrap_or_default(),
                "start.pane" => start.map(|s| s.pane.to_string()).unwrap_or_default(),
                "pane.at" => pane(|p| p.at.clone()),
                "sparkline.bars" => spark(|s| s.bars.to_string()),
                "sparkline.bucket" => spark(|s| s.bucket.clone()),
                "sparkline.metric" => spark(|s| s.metric.clone()),
                "confirm_secs" => num(confirm_secs),
                _ => spark(|s| s.bound.clone()),
            })
            .collect();
        Self {
            row: 0,
            values,
            error: None,
            open: false,
            before: String::new(),
            cursor: usize::MAX,
            session: false,
            columns: columns.map(<[String]>::to_vec),
        }
    }

    /// Select `row`, the cursor after its value.
    pub(super) fn go(&mut self, row: usize) {
        self.row = row;
        self.cursor = usize::MAX;
    }

    /// Open the selected field for editing.
    fn enter(&mut self) {
        self.open = true;
        self.before = self.values[self.row].clone();
        self.cursor = usize::MAX;
    }

    pub(super) fn field(&self) -> &'static Field {
        &FIELDS[self.row]
    }

    /// The values as a policy and the columns list; the error is the one line shown inline on
    /// the field it names.
    #[allow(clippy::type_complexity)]
    fn config(
        &self,
    ) -> Result<
        (
            config::Policy,
            Vec<String>,
            Option<config::Sparkline>,
            Option<config::Pane>,
            Option<config::Start>,
            Option<f64>,
        ),
        String,
    > {
        let v = |name: &str| self.values[field_at(name)].trim();
        let num = |name: &str, what: &str| -> Result<Option<f64>, String> {
            match v(name) {
                "" => Ok(None),
                t => t
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|_| format!("{name}: {what}, not {t:?}")),
            }
        };
        let flag = |name: &str| match v(name) {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        let text = |name: &str| Some(v(name).to_owned()).filter(|t| !t.is_empty());
        let columns = self.columns.clone().unwrap_or_default();
        let policy = config::Policy {
            timeout_min: num("timeout_min", "a number of minutes, as in 30")?,
            budget_usd: num("budget_usd", "dollars, as in 2.00")?,
            daily_budget_usd: num("daily_budget_usd", "dollars, as in 10.00")?,
            write: flag("write"),
            max_turns: match v("max_turns") {
                "" => None,
                t => Some(
                    t.parse::<u32>()
                        .map_err(|_| format!("max_turns: a whole number, as in 5, not {t:?}"))?,
                ),
            },
            codex_full_access: flag("codex_full_access"),
            overlap: match v("overlap") {
                "skip" => Some(config::Overlap::Skip),
                "allow" => Some(config::Overlap::Allow),
                "replace" => Some(config::Overlap::Replace),
                _ => None,
            },
            notify: flag("notify"),
            model: text("model"),
            codex_model: text("codex_model"),
            bedrock: flag("bedrock"),
            aws_profile: text("aws_profile"),
            aws_region: text("aws_region"),
            harness: match v("harness") {
                "claude" => Some(HarnessKind::Claude),
                "codex" => Some(HarnessKind::Codex),
                _ => None,
            },
        };
        // Bedrock with nothing to authenticate it is refused here as the file refuses it, so
        // the session form, which writes nothing and so never reaches `resolve`, cannot set
        // one either. The message names `bedrock`, so it lands on that row.
        config::bedrock_aws(
            policy.bedrock,
            policy.aws_profile.as_deref(),
            policy.aws_region.as_deref(),
        )
        .map_err(|e| format!("{e:#}"))?;
        // The sparkline block: every field empty leaves it out; otherwise the built-in fills
        // what is not typed, and the block is checked the way jobs.yaml is read.
        let spark = if ["bars", "bucket", "metric", "bound"]
            .iter()
            .all(|f| v(&format!("sparkline.{f}")).is_empty())
        {
            None
        } else {
            let built = config::Sparkline::default();
            let s = config::Sparkline {
                bars: match v("sparkline.bars") {
                    "" => built.bars,
                    t => t.parse().map_err(|_| {
                        format!("sparkline.bars: a whole number, as in 16, not {t:?}")
                    })?,
                },
                bucket: text("sparkline.bucket").unwrap_or(built.bucket),
                metric: text("sparkline.metric").unwrap_or(built.metric),
                bound: text("sparkline.bound").unwrap_or(built.bound),
            };
            // Name the field the message is about, so the error lands on it.
            s.check().map_err(|e| {
                let e = format!("{e:#}");
                let field = ["bars", "bucket", "metric", "bound"]
                    .into_iter()
                    .find(|f| e.starts_with(&format!("sparkline {f}")))
                    .unwrap_or("bars");
                format!(
                    "sparkline.{field}: {}",
                    e.trim_start_matches(&format!("sparkline {field} "))
                )
            })?;
            Some(s)
        };
        // The pane block, the same way.
        let pane = if ["at"].iter().all(|f| v(&format!("pane.{f}")).is_empty()) {
            None
        } else {
            let built = config::Pane::default();
            let p = config::Pane {
                at: text("pane.at").unwrap_or(built.at),
            };
            p.check().map_err(|e| {
                let e = format!("{e:#}");
                format!("pane.at: {}", e.trim_start_matches("pane at "))
            })?;
            Some(p)
        };
        // The start block, the same way: no field typed leaves it out of the file.
        let start = if ["harness", "pane"]
            .iter()
            .all(|f| v(&format!("start.{f}")).is_empty())
        {
            None
        } else {
            let built = config::Start::default();
            Some(config::Start {
                harness: match v("start.harness") {
                    "codex" => HarnessKind::Codex,
                    "claude" => HarnessKind::Claude,
                    _ => built.harness,
                },
                pane: flag("start.pane").unwrap_or(built.pane),
            })
        };
        let mark = num("confirm_secs", "seconds, as in 2")?;
        if let Some(m) = mark {
            config::check_confirm_secs(m).map_err(|e| {
                let e = format!("{e:#}");
                format!(
                    "confirm_secs: {}",
                    e.trim_start_matches(&format!("confirm_secs {m}: "))
                )
            })?;
        }
        Ok((policy, columns, spark, pane, start, mark))
    }

    /// `v` as a value the file takes, without the trailing zeros a step leaves behind.
    fn trim_num(v: f64) -> String {
        let s = format!("{v:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    }

    /// `← →` one place along the selected row's control: the next word of a pick's ring, or
    /// the number stepped by its own amount, never below zero. The step lands back on the
    /// step's grid so `0.25` reads `0.25` rather than what the arithmetic left. A built-in
    /// that is not a number, `none` or `system default`, steps from zero. Whether it moved.
    fn turn(&mut self, back: bool) -> bool {
        let f = self.field();
        let value = self.values[self.row].clone();
        if let Some(step) = f.step() {
            let base = if value.is_empty() { f.builtin } else { &value };
            let now: f64 = base.parse().unwrap_or(0.0);
            let next = (now + if back { -step } else { step }).max(0.0);
            self.values[self.row] = Self::trim_num((next / step).round() * step);
            return self.values[self.row] != value;
        }
        let ring = f.ring(&value);
        if ring.is_empty() {
            return false;
        }
        let at = ring.iter().position(|o| *o == value).unwrap_or(0);
        let next = (at + if back { ring.len() - 1 } else { 1 }) % ring.len();
        self.values[self.row] = ring[next].clone();
        next != at
    }

    /// The block written for a value that just changed. The field's own complaint keeps the
    /// cursor where it is and shows inline; another field's moves the cursor to the field it
    /// names, since that is what stopped the block from being written. A value the block
    /// takes goes to the dashboard to write, and a value that did not move writes nothing.
    fn commit(&mut self) -> ConfigAction {
        let changed = self.values[self.row] != self.before;
        match self.config() {
            Err(e) if e.starts_with(&format!("{}:", self.field().name)) => {
                self.error = Some(e);
                ConfigAction::Stay
            }
            Err(e) => {
                self.open = false;
                self.go(FIELDS
                    .iter()
                    .position(|f| e.starts_with(&format!("{}:", f.name)))
                    .unwrap_or(self.row));
                self.error = Some(e);
                ConfigAction::Stay
            }
            Ok((p, c, s, pn, st, m)) => {
                self.open = false;
                if changed {
                    ConfigAction::Save(Box::new(p), c, s, pn, st, m)
                } else {
                    ConfigAction::Stay
                }
            }
        }
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> ConfigAction {
        self.error = None;
        if !self.open {
            match code {
                KeyCode::Esc => return ConfigAction::Cancel,
                // The control is on the row, so `← →` change the value where it is read and
                // the block is written under the key that moved it. There is nothing to open
                // but the text of a typed value, the one control a row cannot draw whole.
                KeyCode::Left | KeyCode::Right => {
                    self.before = self.values[self.row].clone();
                    if self.turn(code == KeyCode::Left) {
                        return self.commit();
                    }
                }
                // Back to the built-in, the one value no ring and no step reaches.
                KeyCode::Backspace
                    if !self.values[self.row].is_empty()
                        && !matches!(self.field().input, Answer::Columns) =>
                {
                    self.before = self.values[self.row].clone();
                    self.values[self.row].clear();
                    return self.commit();
                }
                KeyCode::Enter if matches!(self.field().input, Answer::Columns) => {
                    return ConfigAction::Columns;
                }
                KeyCode::Enter if self.field().typed() => self.enter(),
                KeyCode::Up => {
                    if let Some(r) = (0..self.row).rev().find(|&i| self.shown(i)) {
                        self.go(r);
                    }
                }
                KeyCode::Down => {
                    if let Some(r) = (self.row + 1..FIELDS.len()).find(|&i| self.shown(i)) {
                        self.go(r);
                    }
                }
                // A letter jumps to the word that starts with it.
                KeyCode::Char(c) if !self.field().typed() => {
                    let f = self.field();
                    let opts = f.picks().unwrap_or_default();
                    if let Some(o) = opts.iter().find(|o| f.label(o).starts_with(c)) {
                        self.before = self.values[self.row].clone();
                        self.values[self.row] = if *o == "-" {
                            String::new()
                        } else {
                            (*o).to_owned()
                        };
                        return self.commit();
                    }
                }
                _ => {}
            }
            return ConfigAction::Stay;
        }
        match code {
            KeyCode::Esc => {
                self.values[self.row] = std::mem::take(&mut self.before);
                self.open = false;
            }
            KeyCode::Enter => return self.commit(),
            _ => {
                // Typing over a word the field offers starts from empty rather than
                // appending to it.
                if matches!(self.field().input, Answer::PickOrType(..))
                    && self.field().picked(&self.values[self.row])
                {
                    self.values[self.row].clear();
                }
                if let Some(at) = edit(&mut self.values[self.row], self.cursor, code, mods) {
                    self.cursor = at;
                }
            }
        }
        ConfigAction::Stay
    }

    /// The editor where the list is: a title, the fields under their group headers, each row
    /// name, value and a few words in three columns that hold still whichever row is selected,
    /// the selected row's name lit and its value pressed, then the selected field's fuller
    /// explanation, wrapped to `columns` with the rows' indent and padded to the tallest one so
    /// the block keeps its height.
    fn lines(&self, columns: u16) -> (Vec<Line<'static>>, usize) {
        let title = if self.session {
            (
                "next session",
                "harness, model and provider, from the defaults",
            )
        } else {
            ("config", "jobs.yaml")
        };
        let mut lines = vec![
            Line::default(),
            Line::from(vec![
                Span::styled(title.0, Style::default().fg(ORANGE)),
                Span::styled(format!("  {}", title.1), dim()),
            ]),
        ];
        // The label column is as wide as the widest label on screen, so every control starts
        // in one column and the eye finds them without reading the labels.
        let label_w = (0..FIELDS.len())
            .filter(|&i| self.shown(i))
            .map(|i| FIELDS[i].short.chars().count())
            .max()
            .unwrap_or(0);
        let indent = 4 + label_w + 2;
        let mut head: Option<(&str, &str)> = None;
        // Where the selected row starts, so a list taller than the pane can be drawn from a
        // line that keeps the row being changed on screen.
        let mut at = 0;
        for (i, f) in FIELDS.iter().enumerate() {
            if !self.shown(i) {
                continue;
            }
            // A group's head above its first row, a dim sub-head where a block inside it
            // starts. The session form is one short list under its own title, so it shows
            // neither.
            if !self.session && head.map(|(g, _)| g) != Some(f.group) {
                let (name, what) = GROUPS
                    .iter()
                    .find(|(g, _)| *g == f.group)
                    .copied()
                    .unwrap_or((f.group, ""));
                lines.push(Line::default());
                lines.push(Line::from(vec![
                    Span::styled(name.to_owned(), Style::default().fg(ORANGE)),
                    Span::styled(format!("  {what}"), dim()),
                ]));
            }
            if !self.session && !f.sub.is_empty() && head.map(|(_, b)| b) != Some(f.sub) {
                lines.push(Line::from(Span::styled(format!("  {}", f.sub), dim())));
            }
            head = Some((f.group, f.sub));
            let selected = i == self.row;
            let row = |open| {
                let mut spans = vec![Span::styled(
                    format!("    {:<label_w$}  ", f.short),
                    if selected { lit() } else { bold() },
                )];
                spans.extend(self.control(i, open));
                if selected && let Some(e) = &self.error {
                    spans.push(Span::styled(
                        format!("  {e}"),
                        Style::default().fg(Color::Red),
                    ));
                }
                // A control too wide for the pane takes as many rows as it needs, broken
                // between words and hung under the column it started in. The break comes from
                // the field and the width alone, so a row keeps its height whichever row is
                // selected.
                flow(spans, indent, columns as usize)
            };
            let open = selected && self.open;
            if selected {
                at = lines.len();
            }
            let mut drawn = row(open);
            // A value being typed is a box where its words were, which can be the shorter of
            // the two; the row keeps the height it has shut so the rows under it hold still
            // while it is typed into.
            if open {
                let shut = row(false).len();
                drawn.resize_with(drawn.len().max(shut), Line::default);
            }
            lines.extend(drawn);
        }
        lines.push(Line::default());
        // The explanation wrapped here, not by the widget, so every line of it keeps the
        // rows' indent rather than the second one falling back to the margin. Its height is
        // the tallest field's, so moving down the list moves nothing else.
        let f = self.field();
        let explain = format!("    {:<label_w$}  ", f.name);
        let room = (columns as usize)
            .saturating_sub(explain.chars().count())
            .max(20);
        let tall = FIELDS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.shown(*i))
            .map(|(_, f)| wrap(f.long, room).len())
            .max()
            .unwrap_or(1);
        let mut rest = wrap(f.long, room).into_iter();
        lines.push(Line::from(vec![
            Span::styled(explain, bold()),
            Span::raw(rest.next().unwrap_or_default()),
        ]));
        let mut n = 1;
        for l in rest {
            lines.push(Line::from(format!("    {l}")));
            n += 1;
        }
        lines.extend((n..tall).map(|_| Line::default()));
        (lines, at)
    }

    /// The editor drawn in `body`, scrolled so the selected row is on screen: the list is
    /// taller than the pane, and the row whose control the keys are on has to be the row the
    /// eye can find. The selected row is held in the middle until the ends, which stay put.
    pub(super) fn paragraph(&self, body: Rect) -> Paragraph<'static> {
        let (lines, at) = self.lines(body.width);
        let height = body.height as usize;
        let top = at
            .saturating_sub(height / 2)
            .min(lines.len().saturating_sub(height));
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((top as u16, 0))
    }

    /// The control row `i` draws for its field, the whole of what the field accepts: every
    /// word a pick offers with the current one bracketed and `…` after the ones that also
    /// take something typed, a number between the arrows that step it, or free text in a box.
    /// A value left empty shows the built-in dim in its place. The selected row's open text
    /// carries the cursor, the one control a row cannot draw whole.
    fn control(&self, i: usize, open: bool) -> Vec<Span<'static>> {
        /// Columns the box of a typed value keeps, whatever is in it.
        const BOX_W: usize = 18;
        let (f, value) = (&FIELDS[i], &self.values[i]);
        if matches!(f.input, Answer::Columns) {
            let default: Vec<String> = config::DEFAULT_COLUMNS
                .iter()
                .map(|c| (*c).into())
                .collect();
            let (list, style) = match &self.columns {
                Some(c) if !c.is_empty() => (c.clone(), bold()),
                _ => (default, dim()),
            };
            // One span per column, so a list longer than the pane breaks between names and
            // hangs under the column it started in, as a row of words does.
            let mut spans: Vec<Span<'static>> = list
                .into_iter()
                .map(|c| Span::styled(format!("{c}  "), style))
                .collect();
            spans.push(Span::styled("›", dim()));
            return spans;
        }
        if let Some(opts) = f.picks() {
            let ring = f.ring(value);
            // A value typed into a pick-or-type field stands last in the ring, past the words,
            // so it reads as one more choice rather than as none of them.
            // `-` reads `default` beside the other words, rather than `-` or the whole of
            // `system default`: the row has every word to fit, and the prompt line under it
            // names the built-in the word stands for.
            let labels: Vec<&str> = opts
                .iter()
                .map(|o| if *o == "-" { "default" } else { *o })
                .chain(
                    ring.last()
                        .filter(|v| !opts.contains(&v.as_str()))
                        .map(String::as_str),
                )
                .collect();
            if !open {
                let at = ring.iter().position(|o| o == value).unwrap_or(0);
                let mut spans = vec![];
                picks(&mut spans, &labels, at);
                if matches!(f.input, Answer::PickOrType(..)) {
                    spans.push(Span::styled(" …", dim()));
                }
                return spans;
            }
        }
        if f.step().is_some() {
            // A built-in that is no number of its own, `none` or `system default`, reads
            // `default` between the arrows rather than a word no step could have left there.
            let shown = match (value.is_empty(), f.builtin.parse::<f64>().is_ok()) {
                (false, _) => value,
                (true, true) => f.builtin,
                (true, false) => "default",
            };
            let arrows = if open { lit() } else { dim() };
            return vec![
                Span::styled("‹ ", arrows),
                if open {
                    Span::styled(shown.to_owned(), pressed())
                } else if value.is_empty() {
                    Span::styled(shown.to_owned(), dim())
                } else {
                    Span::styled(shown.to_owned(), bold())
                },
                Span::styled(" ›", arrows),
            ];
        }
        let mut spans = vec![Span::styled("[ ", dim())];
        if open {
            spans.extend(typed(value, self.cursor, f.builtin));
        } else if value.is_empty() {
            let builtin = if f.builtin == SYSTEM {
                "default"
            } else {
                f.builtin
            };
            spans.push(Span::styled(builtin.to_owned(), dim()));
        } else {
            spans.push(Span::styled(value.clone(), bold()));
        }
        let used: usize = spans.iter().skip(1).map(Span::width).sum();
        spans.push(Span::styled(
            format!("{} ]", " ".repeat(BOX_W.saturating_sub(used))),
            dim(),
        ));
        spans
    }

    /// The prompt line: the `jobs.yaml` key the selected row writes, since the row itself
    /// reads as words rather than as the file, then what a value the field can also take
    /// would be and what leaving it empty means. The value is read and changed on the row,
    /// so the line never holds a copy of it.
    pub(super) fn line(&self) -> Line<'static> {
        let f = self.field();
        let mut spans = vec![Span::styled(
            format!("{} › ", f.name),
            Style::default().fg(ORANGE),
        )];
        // Short enough that no field's line wraps in the pane: a line that grew by a row
        // would move the list under it, which is what the controls on the rows are for.
        let default = if f.builtin == SYSTEM {
            "default passes nothing".to_owned()
        } else {
            format!("default: {}", f.builtin)
        };
        let help = if self.open {
            "enter keeps it · esc reverts".to_owned()
        } else {
            match f.input {
                Answer::Columns => "enter arranges them on the table · ctrl+t does too".to_owned(),
                Answer::Pick(_) => default,
                Answer::PickOrType(_, what) => format!("enter types {what} · {default}"),
                _ => format!("enter types it · {default}"),
            }
        };
        spans.push(Span::styled(help, dim()));
        Line::from(spans)
    }
}

/// `spans` as lines of at most `width` columns, broken between spans and every line after the
/// first indented to `indent`, so a control too wide for the pane hangs under the column it
/// started in. The first span keeps at least one span beside it whatever the width, and a span
/// wider than the room it has is left to the widget to clip.
fn flow(spans: Vec<Span<'static>>, indent: usize, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![];
    let mut row: Vec<Span<'static>> = vec![];
    let mut used = 0;
    for span in spans {
        let w = span.width();
        if used + w > width && used > indent {
            lines.push(Line::from(std::mem::take(&mut row)));
            row.push(Span::raw(" ".repeat(indent)));
            used = indent;
        }
        used += w;
        row.push(span);
    }
    if !row.is_empty() {
        lines.push(Line::from(row));
    }
    lines
}

/// `text` broken at spaces into lines of at most `width` columns; a word longer than the
/// width takes a line of its own.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(l) if l.chars().count() + 1 + word.chars().count() <= width => {
                l.push(' ');
                l.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `← →` step a number field on the step's own grid and never below zero, starting from
    /// the built-in where the field is empty and from zero where the built-in is a word no
    /// step could have left there. `backspace` puts the built-in back, the one value no ring
    /// and no step reaches. A control too wide for the pane hangs under the column it started
    /// in, and a row keeps that height whichever row is selected.
    #[test]
    fn arrows_step_a_number_field_on_its_own_grid() {
        let mut c = ConfigForm::new(&config::Policy::default(), None, None, None, None, None);
        let none = KeyModifiers::NONE;
        let value = |c: &ConfigForm| c.values[c.row].clone();

        // budget_usd steps by 0.25 from its built-in, 2.00, and back to a bare 2.
        c.go(field_at("budget_usd"));
        c.key(KeyCode::Right, none);
        assert_eq!(value(&c), "2.25");
        c.key(KeyCode::Right, none);
        assert_eq!(value(&c), "2.5");
        for _ in 0..2 {
            c.key(KeyCode::Left, none);
        }
        assert_eq!(
            value(&c),
            "2",
            "the grid keeps the step's precision, not the float's"
        );
        c.key(KeyCode::Backspace, none);
        assert!(
            value(&c).is_empty(),
            "backspace is the way back to the built-in"
        );

        // `none` is no number, so the first step is one step up from zero, and zero is the
        // floor: a step down from it writes no negative cap.
        c.go(field_at("daily_budget_usd"));
        c.key(KeyCode::Right, none);
        assert_eq!(value(&c), "1");
        for _ in 0..3 {
            c.key(KeyCode::Left, none);
        }
        assert_eq!(value(&c), "0");

        // A pure pick walks its ring and comes back round to the built-in.
        c.go(field_at("overlap"));
        for want in ["skip", "allow", "replace", ""] {
            c.key(KeyCode::Right, none);
            assert_eq!(value(&c), want);
        }
        // A typed value stands last among the words while it is the value, so a step off it
        // is a step to the built-in rather than to the first word; stepping away drops it, as
        // picking another word in a radio group drops what was typed.
        c.go(field_at("model"));
        c.values[c.row] = "claude-opus-5".to_owned();
        c.key(KeyCode::Right, none);
        assert!(value(&c).is_empty(), "past the typed value is the built-in");
        c.key(KeyCode::Left, none);
        assert_eq!(value(&c), "haiku", "and the words alone from there");

        // The columns row reads the list and hands over to the arranger rather than editing
        // it here: there is no table under the editor to pick a set against.
        c.go(field_at("columns"));
        assert!(
            matches!(c.key(KeyCode::Enter, none), ConfigAction::Columns),
            "enter on the columns row asks for the arranger"
        );
        assert!(
            matches!(c.key(KeyCode::Backspace, none), ConfigAction::Stay),
            "and the row has no value of its own to reset"
        );

        let span = |t: &str| Span::raw(t.to_owned());
        let wide = flow(vec![span("ab"), span("cd"), span("ef")], 2, 4);
        assert_eq!(
            wide.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["abcd", "  ef"],
            "a control too wide hangs under the column it started in"
        );
        assert_eq!(
            flow(vec![span("abcdef"), span("gh")], 2, 4).len(),
            2,
            "the first span keeps one span beside it whatever the width"
        );
    }

    /// Every key the guide names is in the dashboard's docs, so the two never drift.
    #[test]
    fn config_explanation_keeps_the_rows_indent() {
        assert_eq!(wrap("a bb ccc dddd", 6), ["a bb", "ccc", "dddd"]);
        assert_eq!(wrap("toolongword x", 4), ["toolongword", "x"]);
        let c = ConfigForm::new(&config::Policy::default(), None, None, None, None, None);
        let (lines, _) = c.lines(48);
        let shown: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(
            shown.iter().any(|l| l.starts_with("runs  ")),
            "group headers sit on the margin"
        );
        assert!(
            shown.iter().any(|l| l.as_str() == "  sparkline"),
            "a block's sub-head is indented by two"
        );
        assert!(
            shown.iter().any(|l| l.starts_with("    time limit (min)")),
            "rows are indented by four and led by their label, under their sub-head"
        );
        let mut tail: Vec<&String> = shown
            .iter()
            .skip_while(|l| !l.starts_with("    time limit (min)  "))
            .skip(1)
            .collect();
        while tail.last().is_some_and(|l| l.is_empty()) {
            tail.pop();
        }
        let long = tail.iter().rev().take_while(|l| !l.is_empty()).count();
        assert!(long > 1, "the explanation wraps at the width given");
        assert!(
            tail.iter()
                .rev()
                .take(long)
                .all(|l| l.starts_with("    ") && l.chars().count() <= 48),
            "every wrapped line keeps the indent and fits"
        );
    }
}
