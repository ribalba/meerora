//! Command scripts of the box, `<data dir>/commands/*.js`, each on a
//! thread of its own with its QuickJS context kept alive between
//! keystrokes (meerora-script `CommandScript`). The UI thread only sends
//! queries and reads answers, so a slow script never holds up typing. A
//! script whose file changed is loaded again, and a broken one reports its
//! error as a row.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant, SystemTime};

use meerora_script::{CommandItem, CommandLimits, CommandMeta, CommandScript};

/// Scripts larger than this are not loaded.
const MAX_SOURCE: u64 = 256 << 10;
/// How often the folder is looked at while the box is open.
const SCAN_EVERY: Duration = Duration::from_millis(800);

/// The scripts of one data dir.
pub struct Scripts {
    dir: PathBuf,
    workers: BTreeMap<PathBuf, Worker>,
    replies: Receiver<Reply>,
    reply_tx: Sender<Reply>,
    last_scan: Option<Instant>,
    seq: u64,
    /// Counts every change the box shows, so it computes its rows only then.
    pub generation: u64,
}

/// What the box knows about one script.
pub struct Worker {
    jobs: Sender<Job>,
    modified: Option<SystemTime>,
    /// The file name, for errors.
    pub name: String,
    /// What `command({...})` declared, once loaded.
    pub meta: Option<CommandMeta>,
    /// Why it did not load.
    pub error: Option<String>,
    /// The query sent last, with its number.
    asked: Option<(u64, String)>,
    /// The newest answer: the query it is for and the items or the error.
    pub answer: Option<(String, Result<Vec<CommandItem>, String>)>,
    /// The number of the query the answer is for.
    answered: u64,
    /// A load was sent and has not been answered.
    loading: bool,
}

enum Job {
    Load,
    Run { seq: u64, query: String },
}

enum Reply {
    Loaded { file: PathBuf, result: Result<CommandMeta, String> },
    Ran { file: PathBuf, seq: u64, query: String, result: Result<Vec<CommandItem>, String> },
}

impl Scripts {
    pub fn new(data_dir: &Path) -> Self {
        let (reply_tx, replies) = channel();
        Self {
            dir: data_dir.join(super::config::SCRIPTS_DIR),
            workers: BTreeMap::new(),
            replies,
            reply_tx,
            last_scan: None,
            seq: 0,
            generation: 0,
        }
    }

    /// Looks for new, changed and removed scripts, at most every
    /// [`SCAN_EVERY`] unless `force` is set. Only directory entries and their
    /// times are read here, the scripts load on their threads.
    pub fn scan(&mut self, force: bool) {
        if !force && self.last_scan.is_some_and(|t| t.elapsed() < SCAN_EVERY) {
            return;
        }
        self.last_scan = Some(Instant::now());
        let mut found: BTreeMap<PathBuf, Option<SystemTime>> = BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "js") && path.is_file() {
                    found.insert(path, entry.metadata().ok().and_then(|m| m.modified().ok()));
                }
            }
        }
        let before = self.workers.len();
        self.workers.retain(|path, _| found.contains_key(path));
        if self.workers.len() != before {
            self.generation += 1;
        }
        for (path, modified) in found {
            match self.workers.get_mut(&path) {
                Some(w) if w.modified == modified => {}
                Some(w) => {
                    w.modified = modified;
                    w.asked = None;
                    w.loading = true;
                    let _ = w.jobs.send(Job::Load);
                }
                None => {
                    let worker = spawn(&path, modified, self.reply_tx.clone());
                    self.workers.insert(path, worker);
                    self.generation += 1;
                }
            }
        }
    }

    /// Takes the answers that arrived.
    pub fn poll(&mut self) {
        loop {
            match self.replies.try_recv() {
                Ok(Reply::Loaded { file, result }) => {
                    self.generation += 1;
                    if let Some(w) = self.workers.get_mut(&file) {
                        w.loading = false;
                        match result {
                            Ok(meta) => {
                                w.meta = Some(meta);
                                w.error = None;
                            }
                            Err(e) => {
                                w.meta = None;
                                w.error = Some(e);
                            }
                        }
                        w.answer = None;
                    }
                }
                Ok(Reply::Ran { file, seq, query, result }) => {
                    if let Some(w) = self.workers.get_mut(&file)
                        && w.asked.as_ref().is_some_and(|(s, _)| *s == seq)
                    {
                        w.answer = Some((query, result));
                        w.answered = seq;
                        self.generation += 1;
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Asks every loaded script whose keyword is `keyword` for `query`,
    /// unless it was asked that already.
    pub fn ask(&mut self, keyword: &str, query: &str) {
        for w in self.workers.values_mut() {
            if w.meta.as_ref().is_none_or(|m| !m.keyword.eq_ignore_ascii_case(keyword)) {
                continue;
            }
            if w.asked.as_ref().is_some_and(|(_, q)| q == query) {
                continue;
            }
            self.seq += 1;
            w.asked = Some((self.seq, query.to_string()));
            let _ = w.jobs.send(Job::Run { seq: self.seq, query: query.to_string() });
        }
    }

    pub fn workers(&self) -> impl Iterator<Item = &Worker> {
        self.workers.values()
    }

    /// Whether every script loaded or failed, and every asked query was
    /// answered, for snapshots and tests.
    pub fn settled(&self) -> bool {
        self.workers.values().all(|w| {
            let loaded = !w.loading && (w.meta.is_some() || w.error.is_some());
            let answered = w.asked.as_ref().is_none_or(|(seq, _)| w.answered == *seq);
            loaded && answered
        })
    }
}

fn spawn(path: &Path, modified: Option<SystemTime>, replies: Sender<Reply>) -> Worker {
    let (jobs, rx) = channel();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let file = path.to_path_buf();
    let thread_name = format!("command-{name}");
    let spawned = std::thread::Builder::new().name(thread_name).spawn(move || work(file, rx, replies));
    let error = spawned.err().map(|e| format!("{name}: cannot start a thread ({e})"));
    let _ = jobs.send(Job::Load);
    let loading = error.is_none();
    Worker { jobs, modified, name, meta: None, error, asked: None, answer: None, answered: 0, loading }
}

/// The thread of one script. Jobs that pile up while a call runs are
/// merged: a pending load first, then only the newest query.
fn work(file: PathBuf, rx: Receiver<Job>, replies: Sender<Reply>) {
    let limits = CommandLimits::default();
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut script: Option<CommandScript> = None;
    while let Ok(first) = rx.recv() {
        let mut load = false;
        let mut run = None;
        for job in std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok())) {
            match job {
                Job::Load => load = true,
                Job::Run { seq, query } => run = Some((seq, query)),
            }
        }
        if load {
            let result = read(&file).and_then(|source| CommandScript::load(&source, &name, &limits).map_err(|e| e.to_string()));
            let reply = match result {
                Ok(s) => {
                    let meta = s.meta().clone();
                    script = Some(s);
                    Ok(meta)
                }
                Err(e) => {
                    script = None;
                    Err(e)
                }
            };
            if replies.send(Reply::Loaded { file: file.clone(), result: reply }).is_err() {
                return;
            }
        }
        if let Some((seq, query)) = run {
            let result = match script.as_mut() {
                Some(s) => s.run(&query).map_err(|e| e.to_string()),
                None => Err(format!("{name} did not load")),
            };
            if replies.send(Reply::Ran { file: file.clone(), seq, query, result }).is_err() {
                return;
            }
        }
    }
}

fn read(file: &Path) -> Result<String, String> {
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let size = std::fs::metadata(file).map_err(|e| format!("{name}: {e}"))?.len();
    if size > MAX_SOURCE {
        return Err(format!("{name} is larger than {} KiB", MAX_SOURCE >> 10));
    }
    std::fs::read_to_string(file).map_err(|e| format!("{name}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data dir of the test's own, removed again even when the test fails.
    struct TempData(PathBuf);

    impl TempData {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("meerora-command-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("commands")).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempData {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn settle(scripts: &mut Scripts) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            scripts.poll();
            if scripts.settled() || Instant::now() > deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Writes `text` so the file's time changes even within one second.
    fn write(path: &Path, text: &str, age_secs: u64) {
        std::fs::write(path, text).unwrap();
        let time = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    #[test]
    fn scripts_answer_reload_and_report_errors() {
        let temp = TempData::new("scripts");
        let data = temp.0.clone();
        let dir = data.join("commands");
        let echo = |prefix: &str| {
            format!(
                "export default command({{ keyword: \"echo\", title: \"Echo\", run(q) {{ return [{{ title: \"{prefix}\" + q, action: copy(q) }}]; }} }});"
            )
        };
        write(&dir.join("echo.js"), &echo("one "), 60);
        write(&dir.join("broken.js"), "export default command({ keyword: ", 60);
        std::fs::write(dir.join("notes.txt"), "not a script").unwrap();

        let mut scripts = Scripts::new(&data);
        scripts.scan(true);
        settle(&mut scripts);
        let names: Vec<&str> = scripts.workers().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["broken.js", "echo.js"]);
        let broken = scripts.workers().find(|w| w.name == "broken.js").unwrap();
        assert!(broken.error.as_deref().is_some_and(|e| e.contains("broken.js")), "{:?}", broken.error);

        scripts.ask("echo", "hi");
        settle(&mut scripts);
        let w = scripts.workers().find(|w| w.name == "echo.js").unwrap();
        let (query, items) = w.answer.clone().unwrap();
        assert_eq!((query.as_str(), items.unwrap()[0].title.as_str()), ("hi", "one hi"));

        // A changed file loads again and answers with the new code.
        write(&dir.join("echo.js"), &echo("two "), 0);
        scripts.scan(true);
        settle(&mut scripts);
        scripts.ask("echo", "hi");
        settle(&mut scripts);
        let w = scripts.workers().find(|w| w.name == "echo.js").unwrap();
        assert_eq!(w.answer.clone().unwrap().1.unwrap()[0].title, "two hi");

        // A removed file goes.
        std::fs::remove_file(dir.join("broken.js")).unwrap();
        scripts.scan(true);
        assert_eq!(scripts.workers().count(), 1);
    }

    #[test]
    fn the_newest_query_wins() {
        let temp = TempData::new("newest");
        let data = temp.0.clone();
        std::fs::write(
            data.join("commands/slow.js"),
            "export default command({ keyword: \"slow\", title: \"Slow\", run(q) { let n = 0; for (let i = 0; i < 2e5; i++) n += i; return [{ title: q }]; } });",
        )
        .unwrap();
        let mut scripts = Scripts::new(&data);
        scripts.scan(true);
        settle(&mut scripts);
        for q in ["a", "ab", "abc", "abcd"] {
            scripts.ask("slow", q);
        }
        settle(&mut scripts);
        let w = scripts.workers().next().unwrap();
        assert_eq!(w.answer.clone().unwrap().1.unwrap()[0].title, "abcd");
    }
}
