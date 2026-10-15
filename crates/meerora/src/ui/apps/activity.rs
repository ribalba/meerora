//! The Activity app of the agent view (D11, D12, D23). It shows the current
//! run with a Stop button, every wake-up of `agent.toml` with the next wake
//! counting down and a Run now button, the tokens used today against the
//! daily budget, the recent runs from `logs/runs.jsonl` in the ring colours
//! of D3, the whole action log with a filter by tool, and the capabilities
//! the agent holds with Revoke (D8, Q4).
//!
//! `state.json` and `logs/runs.jsonl` are read on a thread every second and
//! a half, so the UI never waits for the disk.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, Color32, CornerRadius, FontId, Galley, Id, Painter, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use meerora_protocol::{Command, Outcome, RUNS_LOG, RunRecord, RuntimeKind, ScheduleEntry, Trigger, parse_duration};
use serde::Deserialize;

use super::changes::{Clock, duration};
use crate::engine::Core;
use crate::theme::{self, c, ca};
use crate::ui::{AppTab, ButtonStyle, button, fit, hline, label, pill};
use crate::world::{ActionEntry, RunInfo};

/// Seconds between reads of `state.json` and `logs/runs.jsonl`.
const POLL_EVERY: f64 = 1.5;
/// Runs listed at most.
const RUNS_SHOWN: usize = 100;
/// Seconds a sent Run now shows as starting.
const STARTING_FOR: f64 = 4.0;
const PAD: f32 = 16.0;
const FAILED: [u8; 3] = [0xE0, 0x67, 0x5A];
const SOFT: [u8; 3] = [0xB9, 0xBF, 0xCC];
const BODY: [u8; 3] = [0xC9, 0xCE, 0xD9];

// What the app reads from the agent directory.

/// The parts of `state.json` and `logs/runs.jsonl` the app shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Files {
    /// The local day the token count is for, and the tokens used on it.
    pub tokens: Option<(String, u64)>,
    /// Last wake per schedule entry, unix milliseconds, keyed like the daemon does.
    pub wakes: BTreeMap<String, i64>,
    /// Finished runs, newest first.
    pub runs: Vec<RunRecord>,
}

/// The fields of the daemon's `state.json` (meerora-agentd store.rs) that
/// the app reads. Everything else is ignored.
#[derive(Deserialize, Default)]
#[serde(default)]
struct StateFile {
    tokens: TokenDay,
    schedule: BTreeMap<String, i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TokenDay {
    day: String,
    used: u64,
}

/// Reads `state.json` and `logs/runs.jsonl` of an agent directory. Missing
/// or broken files read as empty.
pub fn read_files(dir: &Path) -> Files {
    let state: StateFile =
        std::fs::read_to_string(dir.join("state.json")).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
    let runs = std::fs::read_to_string(dir.join(RUNS_LOG)).map(|text| parse_runs(&text)).unwrap_or_default();
    let tokens = (!state.tokens.day.is_empty()).then_some((state.tokens.day, state.tokens.used));
    Files { tokens, wakes: state.schedule, runs }
}

/// The runs of `logs/runs.jsonl`, newest first. Broken lines are skipped.
pub fn parse_runs(text: &str) -> Vec<RunRecord> {
    let mut runs: Vec<RunRecord> = text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect();
    runs.reverse();
    runs
}

/// The key under which `state.json` keeps the last wake of an entry, as
/// meerora-agentd schedule.rs `entry_key` makes it.
fn entry_key(entry: &ScheduleEntry) -> String {
    match (&entry.every, &entry.at) {
        (Some(every), _) => format!("every {every}"),
        (None, Some(at)) => format!("at {at}"),
        (None, None) => "never".into(),
    }
}

/// "Every 5 min", "Every hour" or "Every day at 07:00 and 18:30".
pub fn when_text(entry: &ScheduleEntry) -> String {
    match (&entry.every, &entry.at) {
        (Some(every), None) => match parse_duration(every).map(|d| d.as_secs()) {
            Some(60) => "Every minute".into(),
            Some(3600) => "Every hour".into(),
            Some(s) if s % 3600 == 0 => format!("Every {} h", s / 3600),
            Some(s) if s >= 3600 => format!("Every {} h {} min", s / 3600, s % 3600 / 60),
            Some(s) if s % 60 == 0 => format!("Every {} min", s / 60),
            Some(s) if s >= 60 => format!("Every {} min {} s", s / 60, s % 60),
            Some(s) => format!("Every {s} s"),
            None => format!("every = {every:?} is not understood"),
        },
        (None, Some(at)) => {
            let times: Vec<String> = at.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
            let list = match times.as_slice() {
                [] => return format!("at = {at:?} is not understood"),
                [one] => one.clone(),
                [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
            };
            format!("Every day at {list}")
        }
        (Some(_), Some(_)) => "Uses both every and at, so it never runs".into(),
        (None, None) => "Needs every or at, so it never runs".into(),
    }
}

/// The next wake of an `every` entry from its last wake. `at` entries and
/// entries without a recorded wake give `None`.
fn entry_next(entry: &ScheduleEntry, last: Option<i64>) -> Option<i64> {
    let every = parse_duration(entry.every.as_deref()?)?;
    Some(last? + every.as_millis() as i64)
}

/// The group of a tool in the action log: `editor` for `editor.patch`.
pub fn category(tool: &str) -> &str {
    tool.split('.').next().unwrap_or(tool)
}

fn category_name(category: &str) -> String {
    match category {
        "editor" => "Editor".into(),
        "terminal" => "Terminal".into(),
        "browser" => "Browser".into(),
        "files" => "Files".into(),
        "meerora" => "meerora".into(),
        "tool" => "Other tools".into(),
        other => {
            let mut chars = other.chars();
            chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
        }
    }
}

/// Every tool group of `actions` with its count, the most used first.
pub fn categories(actions: &[ActionEntry]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for a in actions {
        *counts.entry(category(&a.tool)).or_default() += 1;
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().map(|(k, n)| (k.to_string(), n)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// `12345` as "12,345".
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// When a run started in unix milliseconds: the start the daemon sent with
/// `RunStarted`, else, from an older daemon, the time the app first saw it.
fn run_start(run: &RunInfo, seen: &mut HashMap<String, i64>, now_ms: i64) -> i64 {
    if run.started > 0 {
        return run.started as i64;
    }
    *seen.entry(run.id.clone()).or_insert(now_ms)
}

/// The full target of a log row for its hover, when the row shows less.
fn row_hover<'a>(action: &'a ActionEntry, shown: &str) -> Option<&'a str> {
    let full = action.target();
    (full != shown).then_some(full)
}

/// Label and colour of an outcome, in the ring colours of D3: blue when it
/// waits for you, dim when it rests, red when it failed.
fn outcome_look(outcome: Outcome) -> (&'static str, [u8; 3]) {
    match outcome {
        Outcome::Done => ("DONE", SOFT),
        Outcome::NeedsYou => ("NEEDS YOU", theme::NEEDS_YOU),
        Outcome::Failed => ("FAILED", FAILED),
        Outcome::Cancelled => ("CANCELLED", theme::RESTING),
    }
}

// The tile.

/// Files read and the action filter of every agent's Activity app.
#[derive(Default)]
pub struct Activity {
    agents: HashMap<String, AgentActivity>,
}

struct AgentActivity {
    dir: PathBuf,
    files: Option<Files>,
    job: Option<Receiver<Files>>,
    polled_at: f64,
    /// The tool group the action log shows, every action when `None`.
    filter: Option<String>,
    /// When the app first saw each run, for runs the daemon sent no start for.
    seen: HashMap<String, i64>,
    /// The wake-up whose Run now was just sent, and when.
    sent: Option<(usize, f64)>,
}

impl AgentActivity {
    fn new(dir: PathBuf) -> Self {
        Self { dir, files: None, job: None, polled_at: f64::NEG_INFINITY, filter: None, seen: HashMap::new(), sent: None }
    }

    fn update(&mut self, now: f64) {
        if let Some(rx) = &self.job {
            match rx.try_recv() {
                Ok(files) => {
                    self.files = Some(files);
                    self.job = None;
                }
                Err(TryRecvError::Disconnected) => self.job = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.job.is_none() && now - self.polled_at >= POLL_EVERY {
            self.polled_at = now;
            let (tx, rx) = mpsc::channel();
            let dir = self.dir.clone();
            match std::thread::Builder::new().name("activity-read".into()).spawn(move || {
                let _ = tx.send(read_files(&dir));
            }) {
                Ok(_) => self.job = Some(rx),
                Err(e) => tracing::warn!("cannot read the activity of {} ({e})", self.dir.display()),
            }
        }
    }
}

/// What a frame shows, copied out of the core so the tile can borrow freely.
struct Cx<'a> {
    id: String,
    name: String,
    run: Option<CurrentRun>,
    activity: String,
    progress: Option<f32>,
    next_wake: Option<(u64, u64)>,
    actions: &'a [ActionEntry],
    world_time: f64,
    schedule: Vec<ScheduleEntry>,
    budget: Option<u64>,
    mock: bool,
    connected: bool,
    granted: Vec<String>,
    system: bool,
    clock: Clock,
}

struct CurrentRun {
    trigger: Trigger,
    task: String,
    started: i64,
}

impl Cx<'_> {
    /// Tokens used today, `None` until `state.json` was read.
    fn used_today(&self, files: Option<&Files>) -> Option<u64> {
        let files = files?;
        let today = self.clock.today();
        Some(files.tokens.as_ref().filter(|(day, _)| *day == today).map_or(0, |(_, used)| *used))
    }

    /// Unix milliseconds of an action, whose time counts from the app's start.
    fn action_ms(&self, a: &ActionEntry) -> i64 {
        self.clock.now_ms - ((self.world_time - a.at).max(0.0) * 1000.0) as i64
    }
}

/// What the user did in a frame.
#[derive(Default)]
struct Act {
    stop: bool,
    run_now: Option<usize>,
    revoke: Option<String>,
    filter: Option<Option<String>>,
}

/// Draws the Activity tile of agent `index`. Returns true while it has keyboard focus.
pub fn tile(ui: &mut Ui, core: &mut Core, index: usize, head: Rect, body: Rect) -> bool {
    let id = core.world.agents[index].id.clone();
    let Some(dir) = core.agent_dir(&id).map(|d| d.path.clone()) else { return false };
    let config = core.agent_dir(&id).map(|d| d.config.clone());
    let now = core.world.time;
    let mut st = core.ui.activity.agents.remove(&id).filter(|s| s.dir == dir).unwrap_or_else(|| AgentActivity::new(dir));
    st.update(now);
    let clock = Clock::now();
    let a = &core.world.agents[index];
    let run = a.run.as_ref().map(|r| {
        let started = run_start(r, &mut st.seen, clock.now_ms);
        CurrentRun { trigger: r.trigger, task: r.task.clone(), started }
    });
    let mock = config.as_ref().and_then(|c| c.runtime.as_ref()).is_none_or(|r| r.kind == RuntimeKind::Mock);
    let cx = Cx {
        id: id.clone(),
        name: a.name.clone(),
        run,
        activity: a.activity.clone(),
        progress: a.progress,
        next_wake: a.next_wake,
        actions: &a.actions,
        world_time: now,
        schedule: config.as_ref().map(|c| c.schedule.clone()).unwrap_or_default(),
        budget: config.as_ref().and_then(|c| c.budget.as_ref()).and_then(|b| b.tokens_per_day),
        mock,
        connected: core.ui.daemon_connected,
        granted: super::super::approvals::granted_of(core, &id),
        system: config.as_ref().is_some_and(|c| c.capabilities.system),
        clock,
    };
    if st.sent.is_some_and(|(_, at)| now - at > STARTING_FOR || cx.run.is_some()) {
        st.sent = None;
    }
    let act = show(ui, &cx, &st, head, body);
    let granted = cx.granted;

    if act.stop
        && let Some(link) = &core.link
    {
        link.send(&Command::Cancel { agent: id.clone() });
    }
    if let Some(index) = act.run_now {
        if let Some(link) = &core.link {
            link.send(&Command::RunNow { agent: id.clone(), schedule: index });
        }
        st.sent = Some((index, now));
    }
    if let Some(capability) = act.revoke {
        // The same as Revoke in the header's Access popover (ui/approvals.rs).
        if let Some(link) = &core.link {
            link.send(&Command::RevokeCapability { agent: id.clone(), capability: capability.clone() });
        }
        let list = core.ui.approvals.granted.entry(id.clone()).or_insert(granted);
        list.retain(|g| *g != capability);
    }
    if let Some(filter) = act.filter {
        st.filter = filter;
    }
    if ui.rect_contains_pointer(body) && ui.input(|i| i.pointer.any_pressed()) {
        core.ui.tab = AppTab::Activity;
    }
    core.ui.activity.agents.insert(id, st);
    false
}

/// Draws the head pill and the sections, in two columns when the tile is
/// wide. Returns what the user did.
fn show(ui: &mut Ui, cx: &Cx, st: &AgentActivity, head: Rect, body: Rect) -> Act {
    if head.width() > 140.0 {
        let (text, color) = match &cx.run {
            Some(_) => ("WORKING".to_string(), theme::WORKING),
            None => match cx.next_wake {
                Some((at, _)) if at as i64 > cx.clock.now_ms => {
                    (format!("WAKES IN {}", duration((at as i64 - cx.clock.now_ms) as u64).to_uppercase()), SOFT)
                }
                _ => ("RESTING".to_string(), SOFT),
            },
        };
        pill(ui.painter(), pos2(head.max.x - 8.0, head.center().y), &text, color);
    }
    let mut act = Act::default();
    if body.width() >= 820.0 {
        let left_w = (body.width() * 0.47).floor();
        let left = Rect::from_min_size(body.min, vec2(left_w, body.height()));
        let right = Rect::from_min_max(pos2(body.min.x + left_w, body.min.y), body.max);
        super::super::vline(&ui.painter_at(body), left.max.x - 0.5, body.min.y, body.max.y);
        column(ui, ("activity-left", &cx.id), left, |ui, w| {
            now_section(ui, cx, st, w, &mut act);
            schedule_section(ui, cx, st, w, &mut act);
            budget_section(ui, cx, st, w);
            access_section(ui, cx, w, &mut act);
        });
        column(ui, ("activity-right", &cx.id), right, |ui, w| {
            runs_section(ui, cx, st, w);
            actions_section(ui, cx, st, w, &mut act);
        });
    } else {
        column(ui, ("activity", &cx.id), body, |ui, w| {
            now_section(ui, cx, st, w, &mut act);
            schedule_section(ui, cx, st, w, &mut act);
            budget_section(ui, cx, st, w);
            runs_section(ui, cx, st, w);
            actions_section(ui, cx, st, w, &mut act);
            access_section(ui, cx, w, &mut act);
        });
    }
    act
}

/// A scrolling column of sections. `add` gets the section width.
fn column(ui: &mut Ui, salt: impl std::hash::Hash + std::fmt::Debug, r: Rect, add: impl FnOnce(&mut Ui, f32)) {
    let mut child = ui.new_child(UiBuilder::new().max_rect(r).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(r.intersect(ui.clip_rect()));
    egui::ScrollArea::vertical().id_salt(salt).auto_shrink([false, false]).content_margin(egui::Margin::ZERO).show(&mut child, |ui| {
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        add(ui, r.width());
        ui.add_space(8.0);
    });
}

/// Allocates a section of height `h` with its label and a line under it.
fn section(ui: &mut Ui, w: f32, h: f32, title: &str) -> Rect {
    let (r, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let painter = ui.painter();
    label(painter, pos2(r.min.x + PAD, r.min.y + 14.0), title);
    hline(painter, r.max.y - 0.5, r.min.x, r.max.x);
    r
}

/// Text in at most `rows` rows of `width`, cut with an ellipsis.
fn paragraph(painter: &Painter, text: &str, font: FontId, color: Color32, width: f32, rows: usize) -> Arc<Galley> {
    let mut job = LayoutJob::simple(text.to_string(), font, color, width.max(10.0));
    job.wrap = TextWrapping { max_width: width.max(10.0), max_rows: rows, break_anywhere: false, overflow_character: Some('…') };
    painter.layout_job(job)
}

/// A small button that does nothing, drawn dim, with the reason on hover.
fn waiting_button(ui: &mut Ui, rect: Rect, label: &str, why: &str, id: Id) {
    let resp = ui.interact(rect, id, Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, CornerRadius::same(10), Color32::TRANSPARENT, Stroke::new(1.0, c([0x22, 0x27, 0x31])), StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, theme::ui(14.0), c([0x5A, 0x60, 0x6E]));
    if !why.is_empty() {
        resp.on_hover_text(why);
    }
}

/// The current run: its task, where it came from, how long it goes and
/// what it does right now, with Stop.
fn now_section(ui: &mut Ui, cx: &Cx, st: &AgentActivity, w: f32, act: &mut Act) {
    let r = section(ui, w, if cx.run.is_some() { 128.0 } else { 100.0 }, "NOW");
    let x = r.min.x + PAD;
    let painter = ui.painter_at(r);
    let Some(run) = &cx.run else {
        let line = match (cx.mock, st.files.as_ref().and_then(|f| f.runs.first())) {
            (true, _) => format!("{} is a mock agent and follows the demo scenario.", cx.name),
            (false, Some(last)) => {
                let (outcome, _) = outcome_look(last.outcome);
                let what = last.summary.clone().unwrap_or_else(|| last.task.lines().next().unwrap_or_default().to_string());
                format!("Last run {} · {} · {what}", cx.clock.when(last.end as i64), outcome.to_lowercase())
            }
            (false, None) => format!("{} has not finished a run yet.", cx.name),
        };
        painter.text(pos2(x, r.min.y + 40.0), Align2::LEFT_TOP, "Resting", theme::ui_semibold(15.5), c(theme::TEXT));
        let galley = paragraph(&painter, &line, theme::ui(13.0), c(theme::TEXT_2), r.width() - 2.0 * PAD, 2);
        painter.galley(pos2(x, r.min.y + 66.0), galley, c(theme::TEXT_2));
        return;
    };
    let stop = Rect::from_min_size(pos2(r.max.x - PAD - 76.0, r.min.y + 38.0), vec2(76.0, 30.0));
    if cx.connected && !cx.mock {
        if button(ui, stop, "Stop", ButtonStyle::Quiet).on_hover_text("Cancels the run").clicked() {
            act.stop = true;
        }
    } else {
        waiting_button(ui, stop, "Stop", "The agent daemon is not connected", Id::new(("activity-stop", &cx.id)));
    }
    let task = run.task.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let galley = paragraph(&painter, task, theme::ui_semibold(15.0), c(theme::TEXT), stop.min.x - x - 12.0, 2);
    let task_h = galley.size().y;
    painter.galley(pos2(x, r.min.y + 38.0), galley, c(theme::TEXT));
    let source = if run.trigger == Trigger::Schedule { "SCHEDULED" } else { "YOU ASKED" };
    let elapsed = duration((cx.clock.now_ms - run.started).max(0) as u64).to_uppercase();
    let y = r.min.y + 44.0 + task_h.max(20.0);
    let s = painter.text(pos2(x, y), Align2::LEFT_TOP, format!("{source} · "), theme::mono(10.5), c(theme::TEXT_2));
    painter.text(pos2(s.max.x, y), Align2::LEFT_TOP, elapsed, theme::mono(10.5), c(theme::WORKING));
    let y = y + 22.0;
    if !cx.activity.is_empty() {
        let line = fit(&painter, &format!("› {}", cx.activity), theme::mono(11.5), r.width() - 2.0 * PAD);
        painter.text(pos2(x, y), Align2::LEFT_TOP, line, theme::mono(11.5), c(BODY));
    }
    if let Some(p) = cx.progress {
        let bar = Rect::from_min_size(pos2(x, r.max.y - 12.0), vec2(r.width() - 2.0 * PAD, 3.0));
        painter.rect_filled(bar, CornerRadius::same(2), c(theme::BORDER));
        painter.rect_filled(bar.with_max_x(bar.min.x + bar.width() * p.clamp(0.0, 1.0)), CornerRadius::same(2), c(theme::WORKING));
    }
}

/// Every wake-up of `agent.toml` with Run now, under the countdown to the next wake.
fn schedule_section(ui: &mut Ui, cx: &Cx, st: &AgentActivity, w: f32, act: &mut Act) {
    const ROW: f32 = 70.0;
    let rows = cx.schedule.len().max(1) as f32;
    let r = section(ui, w, 44.0 + 46.0 + ROW * rows + 6.0, "SCHEDULE");
    let x = r.min.x + PAD;
    let inner_w = r.width() - 2.0 * PAD;
    let painter = ui.painter_at(r);

    // The next wake, as the dim ring counts it down (D3, D23).
    let mut y = r.min.y + 38.0;
    match cx.next_wake {
        Some((at, since)) => {
            let (at, since) = (at as i64, since as i64);
            let left = at - cx.clock.now_ms;
            let text = if left > 0 { format!("Next wake in {}", duration(left as u64)) } else { "Waking now".to_string() };
            let t = painter.text(pos2(x, y), Align2::LEFT_TOP, text, theme::ui_semibold(14.5), c(theme::TEXT));
            painter.text(
                pos2(t.max.x + 10.0, y + 2.0),
                Align2::LEFT_TOP,
                format!("at {}", cx.clock.when(at)),
                theme::ui(13.0),
                c(theme::TEXT_2),
            );
            let bar = Rect::from_min_size(pos2(x, y + 26.0), vec2(inner_w, 3.0));
            painter.rect_filled(bar, CornerRadius::same(2), ca(theme::RESTING, 90));
            let share = (left as f32 / (at - since).max(1) as f32).clamp(0.0, 1.0);
            painter.rect_filled(bar.with_max_x(bar.min.x + bar.width() * share), CornerRadius::same(2), c(theme::TEXT_2));
        }
        None => {
            let text = if cx.schedule.is_empty() {
                "No wake-ups"
            } else if cx.mock {
                "The demo scenario wakes mock agents"
            } else {
                "No wake is planned"
            };
            painter.text(pos2(x, y), Align2::LEFT_TOP, text, theme::ui_semibold(14.5), c(theme::TEXT));
        }
    }
    y += 46.0;

    if cx.schedule.is_empty() {
        let text = format!("{} only works when you ask. Wake-ups are set in Edit agent or in [[schedule]] of agent.toml.", cx.name);
        let galley = paragraph(&painter, &text, theme::ui(13.0), c(theme::TEXT_2), inner_w, 3);
        painter.galley(pos2(x, y), galley, c(theme::TEXT_2));
        return;
    }
    let files = st.files.as_ref();
    let used = cx.used_today(files);
    let over = matches!((cx.budget, used), (Some(limit), Some(used)) if used >= limit);
    let next_at = cx.next_wake.map(|(at, _)| at as i64);
    for (i, entry) in cx.schedule.iter().enumerate() {
        let row = Rect::from_min_size(pos2(r.min.x, y), vec2(r.width(), ROW - 6.0));
        let painter = ui.painter_at(r);
        painter.rect_filled(row.shrink2(vec2(10.0, 0.0)), CornerRadius::same(10), c([0x15, 0x19, 0x22]));
        let b = Rect::from_min_size(pos2(row.max.x - PAD - 96.0, row.min.y + 10.0), vec2(96.0, 28.0));
        let why = if cx.mock {
            format!("{} is a mock agent", cx.name)
        } else if !cx.connected {
            "The agent daemon is not connected".to_string()
        } else if cx.run.is_some() {
            format!("{} is working on a run", cx.name)
        } else if over {
            "Today's token budget is used up".to_string()
        } else {
            String::new()
        };
        let bid = Id::new(("activity-run-now", &cx.id, i));
        if st.sent.is_some_and(|(sent, _)| sent == i) {
            waiting_button(ui, b, "Starting…", "", bid);
        } else if !why.is_empty() {
            waiting_button(ui, b, "Run now", &why, bid);
        } else if button(ui, b, "Run now", ButtonStyle::Quiet).on_hover_text("Starts this wake-up's task now").clicked() {
            act.run_now = Some(i);
        }
        let last = files.and_then(|f| f.wakes.get(&entry_key(entry)).copied());
        let is_next = matches!((entry_next(entry, last), next_at), (Some(a), Some(b)) if (a - b).abs() < 120_000);
        let text_x = row.min.x + PAD + 8.0;
        let room = b.min.x - text_x - 10.0;
        let when = when_text(entry);
        let wr = painter.text(
            pos2(text_x, row.min.y + 10.0),
            Align2::LEFT_TOP,
            fit(&painter, &when, theme::ui_semibold(14.0), room),
            theme::ui_semibold(14.0),
            c(theme::TEXT),
        );
        if is_next && wr.max.x + 60.0 < b.min.x {
            pill(&painter, pos2(wr.max.x + 8.0 + 44.0, wr.center().y), "NEXT", SOFT);
        }
        let task = entry.task.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
        let task = fit(&painter, task, theme::ui(13.0), room);
        painter.text(pos2(text_x, row.min.y + 30.0), Align2::LEFT_TOP, task, theme::ui(13.0), c(BODY));
        let mut meta = Vec::new();
        if let Some(quiet) = &entry.quiet_hours {
            meta.push(format!("QUIET {}", quiet.replace('-', " TO ")));
        }
        if let Some(station) = &entry.station {
            meta.push(format!("AT THE {}", station.replace('_', " ").to_uppercase()));
        }
        if let Some(last) = last {
            meta.push(format!("LAST {}", cx.clock.when(last).to_uppercase()));
        }
        if !meta.is_empty() {
            let meta = fit(&painter, &meta.join(" · "), theme::mono(10.5), row.max.x - PAD - 8.0 - text_x);
            painter.text(pos2(text_x, row.min.y + 48.0), Align2::LEFT_TOP, meta, theme::mono(10.5), c(theme::TEXT_2));
        }
        y += ROW;
    }
}

/// Tokens used today against `tokens_per_day`.
fn budget_section(ui: &mut Ui, cx: &Cx, st: &AgentActivity, w: f32) {
    let r = section(ui, w, 124.0, "BUDGET");
    let x = r.min.x + PAD;
    let inner_w = r.width() - 2.0 * PAD;
    let painter = ui.painter_at(r);
    let used = cx.used_today(st.files.as_ref());
    let used_text = used.map_or_else(|| "…".to_string(), thousands);
    let Some(limit) = cx.budget else {
        painter.text(pos2(x, r.min.y + 38.0), Align2::LEFT_TOP, "No daily budget", theme::ui_semibold(14.5), c(theme::TEXT));
        let line = format!("{used_text} tokens used today. Set tokens_per_day under [budget] in agent.toml to limit scheduled runs.");
        let galley = paragraph(&painter, &line, theme::ui(13.0), c(theme::TEXT_2), inner_w, 2);
        painter.galley(pos2(x, r.min.y + 62.0), galley, c(theme::TEXT_2));
        return;
    };
    let share = used.map_or(0.0, |u| u as f32 / limit.max(1) as f32);
    let text = format!("{used_text} of {} tokens today", thousands(limit));
    painter.text(pos2(x, r.min.y + 38.0), Align2::LEFT_TOP, text, theme::ui_semibold(14.5), c(theme::TEXT));
    let color = if share >= 1.0 { FAILED } else { theme::WORKING };
    let percent = format!("{}%", (share * 100.0).round() as u32);
    painter.text(pos2(r.max.x - PAD, r.min.y + 40.0), Align2::RIGHT_TOP, percent, theme::mono(11.0), c(color));
    let bar = Rect::from_min_size(pos2(x, r.min.y + 64.0), vec2(inner_w, 6.0));
    painter.rect_filled(bar, CornerRadius::same(3), c(theme::BORDER));
    painter.rect_filled(bar.with_max_x(bar.min.x + bar.width() * share.clamp(0.0, 1.0)), CornerRadius::same(3), c(color));
    let note = if share >= 1.0 {
        "Used up. Scheduled runs wait until tomorrow, and your own messages still start runs."
    } else {
        "Scheduled runs wait when it is used up. Your own messages still start runs."
    };
    let galley = paragraph(&painter, note, theme::ui(12.5), c(theme::TEXT_2), inner_w, 2);
    painter.galley(pos2(x, r.min.y + 80.0), galley, c(theme::TEXT_2));
}

/// The finished runs from `logs/runs.jsonl`, newest first.
fn runs_section(ui: &mut Ui, cx: &Cx, st: &AgentActivity, w: f32) {
    const ROW: f32 = 66.0;
    let runs: &[RunRecord] = st.files.as_ref().map_or(&[], |f| &f.runs);
    let shown = runs.len().min(RUNS_SHOWN);
    let more = runs.len() - shown;
    let h = 42.0 + if shown == 0 { 52.0 } else { ROW * shown as f32 } + if more > 0 { 28.0 } else { 6.0 };
    let r = section(ui, w, h, "RECENT RUNS");
    let x = r.min.x + PAD;
    let inner_w = r.width() - 2.0 * PAD;
    if shown == 0 {
        let painter = ui.painter_at(r);
        let text = if st.files.is_none() { "Reading…".to_string() } else { format!("{} has no finished runs yet.", cx.name) };
        let galley = paragraph(&painter, &text, theme::ui(13.0), c(theme::TEXT_2), inner_w, 2);
        painter.galley(pos2(x, r.min.y + 38.0), galley, c(theme::TEXT_2));
        return;
    }
    let mut y = r.min.y + 36.0;
    for run in &runs[..shown] {
        let row = Rect::from_min_size(pos2(r.min.x, y), vec2(r.width(), ROW));
        y += ROW;
        if !ui.is_rect_visible(row) {
            continue;
        }
        let painter = ui.painter_at(r);
        let (outcome, color) = outcome_look(run.outcome);
        // The dot takes the ring colour of the outcome (D3).
        let dot = pos2(x + 5.0, row.min.y + 13.0);
        match run.outcome {
            Outcome::Done | Outcome::Cancelled => painter.circle(dot, 4.5, c(theme::PANEL), Stroke::new(1.6, c(color))),
            _ => painter.circle_filled(dot, 5.0, c(color)),
        };
        let pill_r = pill(&painter, pos2(r.max.x - PAD, row.min.y + 13.0), outcome, color);
        let tx = x + 20.0;
        let task = run.task.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
        let task = fit(&painter, task, theme::ui(14.0), pill_r.min.x - tx - 10.0);
        painter.text(pos2(tx, row.min.y + 13.0), Align2::LEFT_CENTER, task, theme::ui(14.0), c(theme::TEXT));
        if let Some(summary) = &run.summary {
            let summary = fit(&painter, summary, theme::ui(12.5), r.max.x - PAD - tx);
            painter.text(pos2(tx, row.min.y + 33.0), Align2::LEFT_CENTER, summary, theme::ui(12.5), c(theme::TEXT_2));
        }
        let source = if run.trigger == Trigger::Schedule { "SCHEDULED" } else { "YOU ASKED" };
        let mut meta = format!(
            "{source} · {} · {}",
            cx.clock.when(run.start as i64).to_uppercase(),
            duration(run.end.saturating_sub(run.start)).to_uppercase()
        );
        if run.tokens > 0 {
            meta.push_str(&format!(" · {} TOKENS", thousands(run.tokens)));
        }
        let meta = fit(&painter, &meta, theme::mono(10.5), r.max.x - PAD - tx);
        painter.text(pos2(tx, row.min.y + 51.0), Align2::LEFT_CENTER, meta, theme::mono(10.5), c([0x7A, 0x82, 0x92]));
    }
    if more > 0 {
        let painter = ui.painter_at(r);
        let text = format!("{more} older runs are in {RUNS_LOG}.");
        painter.text(pos2(x, r.max.y - 18.0), Align2::LEFT_CENTER, text, theme::ui(12.5), c(theme::TEXT_2));
    }
}

/// Every action of the agent on your apps, newest first, filtered by tool group.
fn actions_section(ui: &mut Ui, cx: &Cx, st: &AgentActivity, w: f32, act: &mut Act) {
    const ROW: f32 = 24.0;
    const CHIP_H: f32 = 26.0;
    let groups = categories(cx.actions);
    let filter = st.filter.as_deref().filter(|f| groups.iter().any(|(g, _)| g == f));
    let shown: Vec<&ActionEntry> = cx.actions.iter().rev().filter(|a| filter.is_none_or(|f| category(&a.tool) == f)).collect();

    // The chips: All, then every tool group, wrapped into rows.
    let font = theme::ui(13.0);
    let mut chips = vec![(None, format!("All {}", cx.actions.len()))];
    chips.extend(groups.iter().map(|(g, n)| (Some(g.clone()), format!("{} {n}", category_name(g)))));
    let inner_w = w - 2.0 * PAD;
    let widths: Vec<f32> =
        chips.iter().map(|(_, t)| ui.painter().layout_no_wrap(t.clone(), font.clone(), Color32::WHITE).size().x + 22.0).collect();
    let mut chip_rows = 1;
    let mut line_w = 0.0;
    for cw in &widths {
        if line_w > 0.0 && line_w + cw > inner_w {
            chip_rows += 1;
            line_w = 0.0;
        }
        line_w += cw + 6.0;
    }
    let chips_h = if cx.actions.is_empty() { 0.0 } else { chip_rows as f32 * (CHIP_H + 6.0) + 6.0 };
    let list_h = if shown.is_empty() { 44.0 } else { ROW * shown.len() as f32 };
    let r = section(ui, w, 40.0 + chips_h + list_h + 10.0, "ACTIONS ON YOUR APPS");
    let x = r.min.x + PAD;
    let mut chip_x = x;
    let mut y = r.min.y + 36.0;
    if !cx.actions.is_empty() {
        for ((key, text), cw) in chips.iter().zip(&widths) {
            if chip_x > x && chip_x + cw > r.max.x - PAD {
                chip_x = x;
                y += CHIP_H + 6.0;
            }
            let chip = Rect::from_min_size(pos2(chip_x, y), vec2(*cw, CHIP_H));
            let selected = key.as_deref() == filter;
            let resp = ui.interact(chip, Id::new(("activity-chip", &cx.id, key)), Sense::click());
            let painter = ui.painter();
            let fill = if selected {
                c([0x1E, 0x23, 0x2E])
            } else if resp.hovered() {
                c([0x1A, 0x1F, 0x2A])
            } else {
                Color32::TRANSPARENT
            };
            let stroke = if selected { Stroke::new(1.0, c([0x3A, 0x42, 0x52])) } else { Stroke::new(1.0, c([0x2A, 0x30, 0x3D])) };
            painter.rect(chip, CornerRadius::same(13), fill, stroke, StrokeKind::Inside);
            let color = if selected { c(theme::TEXT) } else { c(theme::TEXT_2) };
            painter.text(chip.center(), Align2::CENTER_CENTER, text, font.clone(), color);
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                act.filter = Some(key.clone());
            }
            chip_x += cw + 6.0;
        }
        y += CHIP_H + 12.0;
    }
    let painter = ui.painter_at(r);
    if shown.is_empty() {
        let text = format!("Nothing yet. What {} does in the editor, the terminal and the browser shows here.", cx.name);
        let galley = paragraph(&painter, &text, theme::ui(13.0), c(theme::TEXT_2), r.width() - 2.0 * PAD, 2);
        painter.galley(pos2(x, y), galley, c(theme::TEXT_2));
        return;
    }
    let time_w = painter.layout_no_wrap("00:00:00".into(), theme::mono(10.5), Color32::WHITE).size().x;
    let tool_w =
        shown.iter().map(|a| painter.layout_no_wrap(a.tool.clone(), theme::mono(11.5), Color32::WHITE).size().x).fold(0.0, f32::max);
    let tool_w = tool_w.min((r.width() * 0.3).max(90.0));
    for (i, a) in shown.into_iter().enumerate() {
        let row = Rect::from_min_size(pos2(r.min.x, y), vec2(r.width(), ROW));
        y += ROW;
        if !ui.is_rect_visible(row) {
            continue;
        }
        let when = cx.clock.clock(cx.action_ms(a));
        painter.text(pos2(x, row.center().y), Align2::LEFT_CENTER, when, theme::mono(10.5), c(theme::TEXT_2));
        let tx = x + time_w + 14.0;
        let tool = fit(&painter, &a.tool, theme::mono(11.5), tool_w);
        painter.text(pos2(tx, row.center().y), Align2::LEFT_CENTER, tool, theme::mono(11.5), c(SOFT));
        let sx = tx + tool_w + 14.0;
        let summary = fit(&painter, a.summary.lines().next().unwrap_or_default(), theme::mono(11.5), r.max.x - PAD - sx);
        // The log keeps the short line, the hover offers the full command, path or URL.
        if let Some(full) = row_hover(a, &summary) {
            ui.interact(row, Id::new(("activity-action", &cx.id, i)), Sense::hover()).on_hover_text(full);
        }
        painter.text(pos2(sx, row.center().y), Align2::LEFT_CENTER, summary, theme::mono(11.5), c(BODY));
    }
}

/// What the agent may do, each with Revoke as in the header's Access popover
/// (D8, Q4). It is the last section of its column.
fn access_section(ui: &mut Ui, cx: &Cx, w: f32, act: &mut Act) {
    const ROW: f32 = 52.0;
    let rows = cx.granted.len() as f32 + if cx.system { 1.0 } else { 0.0 };
    let h = 40.0 + if rows == 0.0 { 50.0 } else { ROW * rows } + 8.0;
    let r = section(ui, w, h, "ACCESS");
    // The last section needs no line under it.
    ui.painter().rect_filled(Rect::from_min_max(pos2(r.min.x, r.max.y - 1.0), r.max), CornerRadius::ZERO, c(theme::PANEL));
    let x = r.min.x + PAD;
    let inner_w = r.width() - 2.0 * PAD;
    let mut y = r.min.y + 36.0;
    if rows == 0.0 {
        let painter = ui.painter_at(r);
        let text = format!("Nothing granted. {} reads and edits its own folder and asks when a task needs more.", cx.name);
        let galley = paragraph(&painter, &text, theme::ui(13.0), c(theme::TEXT_2), inner_w, 2);
        painter.galley(pos2(x, y), galley, c(theme::TEXT_2));
        return;
    }
    if cx.system {
        let row = Rect::from_min_size(pos2(x, y), vec2(inner_w, ROW - 6.0));
        let painter = ui.painter_at(r);
        painter.rect_filled(row, CornerRadius::same(10), ca(theme::SYSTEM, 22));
        painter.text(pos2(row.min.x + 14.0, row.min.y + 8.0), Align2::LEFT_TOP, "system", theme::mono(12.0), c(theme::SYSTEM_2));
        let text = fit(&painter, "Acts with system privileges, set in agent.toml", theme::ui(12.5), row.width() - 28.0);
        painter.text(pos2(row.min.x + 14.0, row.min.y + 25.0), Align2::LEFT_TOP, text, theme::ui(12.5), c(theme::TEXT_2));
        y += ROW;
    }
    for capability in &cx.granted {
        let row = Rect::from_min_size(pos2(x, y), vec2(inner_w, ROW - 6.0));
        y += ROW;
        let b = Rect::from_min_size(pos2(row.max.x - 8.0 - 74.0, row.center().y - 14.0), vec2(74.0, 28.0));
        let painter = ui.painter_at(r);
        painter.rect_filled(row, CornerRadius::same(10), c([0x1B, 0x20, 0x30]));
        super::super::approvals::lock(&painter, pos2(row.min.x + 16.0, row.min.y + 15.0), 12.0, c(theme::SYSTEM_2));
        let text_w = b.min.x - row.min.x - 38.0;
        let name = fit(&painter, capability, theme::mono(12.0), text_w);
        painter.text(pos2(row.min.x + 30.0, row.min.y + 8.0), Align2::LEFT_TOP, name, theme::mono(12.0), c(theme::TEXT));
        let described = super::super::approvals::describe(capability);
        let mut chars = described.chars();
        let sentence: String = chars.next().map(|f| f.to_uppercase().chain(chars).collect()).unwrap_or_default();
        let plain = fit(&painter, &sentence, theme::ui(12.5), text_w);
        painter.text(pos2(row.min.x + 30.0, row.min.y + 25.0), Align2::LEFT_TOP, plain, theme::ui(12.5), c(theme::TEXT_2));
        if cx.connected {
            if button(ui, b, "Revoke", ButtonStyle::Quiet).on_hover_text("Revoking stops a run that uses it").clicked() {
                act.revoke = Some(capability.clone());
            }
        } else {
            waiting_button(ui, b, "Revoke", "The agent daemon is not connected", Id::new(("activity-revoke", &cx.id, capability)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::editor::review::tests::TempDir;

    fn record(id: &str, outcome: Outcome) -> String {
        let r = RunRecord {
            id: id.into(),
            trigger: Trigger::Schedule,
            task: "Check the inbox.".into(),
            start: 1_000,
            end: 61_000,
            outcome,
            summary: Some("Nothing new.".into()),
            tokens: 3210,
        };
        serde_json::to_string(&r).unwrap()
    }

    #[test]
    fn runs_parse_newest_first_and_skip_broken_lines() {
        let text = format!("{}\nnot json\n\n{}\n{{\"id\":\"half\"}}\n", record("wren-1", Outcome::Done), record("wren-2", Outcome::Failed));
        let runs = parse_runs(&text);
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["wren-2", "wren-1"]);
        assert_eq!((runs[0].outcome, runs[0].tokens, runs[0].summary.as_deref()), (Outcome::Failed, 3210, Some("Nothing new.")));
        // The daemon's line format, written by meerora-agentd store.rs.
        let line = r#"{"id":"wren-1791991823000","trigger":"user","task":"Hi","start":1,"end":2,"outcome":"needs_you","tokens":5}"#;
        let run = &parse_runs(line)[0];
        assert_eq!((run.trigger, run.outcome, run.summary.as_deref()), (Trigger::User, Outcome::NeedsYou, None));
    }

    #[test]
    fn the_run_start_comes_from_the_daemon() {
        let mut seen = HashMap::new();
        let run = |id: &str, started| RunInfo { id: id.into(), trigger: Trigger::User, task: "t".into(), started };
        assert_eq!(run_start(&run("wren-1", 1_791_991_823_000), &mut seen, 5), 1_791_991_823_000);
        assert!(seen.is_empty());
        // An older daemon sends no start: the first sight counts and stays.
        assert_eq!(run_start(&run("r2", 0), &mut seen, 7), 7);
        assert_eq!(run_start(&run("r2", 0), &mut seen, 9), 7);
    }

    #[test]
    fn log_rows_offer_the_full_target_on_hover() {
        let url = format!("https://example.org/{}", "deep/".repeat(30));
        let cut: String = url.chars().take(79).chain(['…']).collect();
        let long = ActionEntry { at: 0.0, tool: "browser.open".into(), summary: cut.clone(), target: Some(url.clone()) };
        assert_eq!(row_hover(&long, &cut), Some(url.as_str()));
        let short = ActionEntry { at: 0.0, tool: "terminal.run".into(), summary: "make".into(), target: None };
        assert_eq!(row_hover(&short, "make"), None, "nothing more to show");
        assert_eq!(row_hover(&short, "ma…"), Some("make"), "cut by the row width");
    }

    #[test]
    fn files_of_an_agent_directory() {
        let tmp = TempDir::new("activity-files");
        assert_eq!(read_files(&tmp.0), Files::default(), "nothing there yet");
        std::fs::create_dir_all(tmp.0.join("logs")).unwrap();
        std::fs::write(tmp.0.join(RUNS_LOG), format!("{}\n", record("wren-1", Outcome::Done))).unwrap();
        std::fs::write(
            tmp.0.join("state.json"),
            r#"{"session":"s1","tokens":{"day":"2026-10-14","used":1234},"schedule":{"every 5m":1791991823000},"params":{}}"#,
        )
        .unwrap();
        let files = read_files(&tmp.0);
        assert_eq!(files.tokens, Some(("2026-10-14".into(), 1234)));
        assert_eq!(files.wakes["every 5m"], 1_791_991_823_000);
        assert_eq!(files.runs.len(), 1);
    }

    #[test]
    fn the_tile_draws_without_a_window() {
        let tmp = TempDir::new("activity-draw");
        std::fs::create_dir_all(tmp.0.join("logs")).unwrap();
        let runs: Vec<String> = (0..120).map(|i| record(&format!("wren-{i}"), Outcome::Done)).collect();
        std::fs::write(tmp.0.join(RUNS_LOG), runs.join("\n")).unwrap();
        std::fs::write(tmp.0.join("state.json"), r#"{"tokens":{"day":"2000-01-01","used":99}}"#).unwrap();
        let mut st = AgentActivity::new(tmp.0.clone());
        st.files = Some(read_files(&tmp.0));
        st.filter = Some("editor".into());
        let actions: Vec<ActionEntry> = ["editor.patch", "terminal.run", "browser.open", "tool.Grep"]
            .iter()
            .map(|t| ActionEntry {
                at: 1.0,
                tool: (*t).into(),
                summary: "memory/notes.md +1 -1".into(),
                target: Some("memory/notes.md".into()),
            })
            .collect();
        let schedule = vec![
            ScheduleEntry {
                every: Some("5m".into()),
                at: None,
                task: "Check the inbox.".into(),
                station: Some("mailbox".into()),
                quiet_hours: Some("22:00-07:00".into()),
            },
            ScheduleEntry { every: None, at: Some("07:30".into()), task: "Digest".into(), station: None, quiet_hours: None },
        ];
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(1200.0, 800.0));
        for (width, running) in [(1000.0, true), (400.0, false), (1000.0, false)] {
            let cx = Cx {
                id: "wren".into(),
                name: "Wren".into(),
                run: running.then(|| CurrentRun { trigger: Trigger::Schedule, task: "Check the inbox.".into(), started: 0 }),
                activity: "reading mail".into(),
                progress: running.then_some(0.4),
                next_wake: Some((u64::MAX / 4, 0)),
                actions: &actions,
                world_time: 2.0,
                schedule: if running { schedule.clone() } else { Vec::new() },
                budget: running.then_some(100),
                mock: false,
                connected: true,
                granted: vec!["web".into(), "mail.read".into()],
                system: running,
                clock: Clock::now(),
            };
            assert_eq!(cx.used_today(st.files.as_ref()), Some(0), "the tokens of another day do not count");
            let raw = egui::RawInput { screen_rect: Some(screen), ..Default::default() };
            let mut output = ctx.run_ui(raw, |ui| {
                let head = Rect::from_min_size(pos2(10.0, 10.0), vec2(width, 38.0));
                let body = Rect::from_min_max(pos2(11.0, 49.0), pos2(10.0 + width, 790.0));
                let act = show(ui, &cx, &st, head, body);
                assert!(!act.stop && act.run_now.is_none() && act.revoke.is_none());
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn schedule_and_action_texts() {
        let entry = |every: Option<&str>, at: Option<&str>| ScheduleEntry {
            every: every.map(Into::into),
            at: at.map(Into::into),
            task: "t".into(),
            station: None,
            quiet_hours: None,
        };
        assert_eq!(when_text(&entry(Some("5m"), None)), "Every 5 min");
        assert_eq!(when_text(&entry(Some("1h"), None)), "Every hour");
        assert_eq!(when_text(&entry(Some("1h30m"), None)), "Every 1 h 30 min");
        assert_eq!(when_text(&entry(Some("40s"), None)), "Every 40 s");
        assert_eq!(when_text(&entry(None, Some("07:00, 12:00, 18:30"))), "Every day at 07:00, 12:00 and 18:30");
        assert_eq!(entry_key(&entry(Some("5m"), None)), "every 5m");
        assert_eq!(entry_next(&entry(Some("5m"), None), Some(1_000)), Some(301_000));
        assert_eq!(entry_next(&entry(None, Some("07:00")), Some(1_000)), None);

        let a = |tool: &str| ActionEntry { at: 0.0, tool: tool.into(), summary: String::new(), target: None };
        let actions = [a("editor.patch"), a("terminal.run"), a("editor.patch"), a("tool.Grep"), a("browser.open")];
        assert_eq!(categories(&actions), [("editor".to_string(), 2), ("browser".into(), 1), ("terminal".into(), 1), ("tool".into(), 1)]);
        assert_eq!(category_name("tool"), "Other tools");
        assert_eq!(category_name("mail"), "Mail");
        assert_eq!(thousands(200000), "200,000");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
