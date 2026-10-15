//! LLM runtimes behind one interface (D22).
//!
//! *Harness runtimes* bring their own agent loop and tools: the Claude Code
//! CLI and the Codex CLI. *Model runtimes* are plain models for which this
//! crate runs the loop itself: Ollama. Every runtime gets the meerora tools
//! through the MCP bridge (`meerora-mcp`), and every runtime reports the same
//! [`RunEvent`]s, so the daemon treats them alike.
//!
//! Permissions follow D29. Without capabilities an agent may only read and
//! edit its own directory and call the meerora tools. `shell` allows commands
//! in that directory, `web` allows fetching and searching. Runs must not load
//! the user's personal CLI configuration (memory files, other MCP servers),
//! because it can hold secrets that have nothing to do with the agent.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use meerora_protocol::{RuntimeConfig, RuntimeKind, Settings};

pub mod actions;
mod bounded;
mod claude;
mod codex;
mod external;
mod homes;
mod mcp;
mod ollama;
mod process;
pub mod sandbox;
mod tools;

pub use external::{A2A_VERSION, CARD_FILE as EXTERNAL_CARD_FILE};
pub use ollama::{DEFAULT_URL as DEFAULT_OLLAMA_URL, MAX_TURNS as OLLAMA_MAX_TURNS};

/// How long a run waits for the user to decide a capability request (D8).
/// A little longer than the MCP bridge waits, so the bridge's own answer
/// reaches the model first.
pub const DECISION_WAIT: std::time::Duration = std::time::Duration::from_secs(11 * 60);

/// What one run gets.
#[derive(Clone, Debug, Default)]
pub struct RunRequest {
    pub agent: String,
    /// The agent directory (D21). Working directory of the run and the only
    /// place it may write.
    pub dir: PathBuf,
    /// The user's message or the scheduled task.
    pub prompt: String,
    /// System prompt: meerora's preamble plus the agent's instructions.
    pub system: String,
    /// Runtime session to continue, from an earlier [`RunEvent::Session`].
    ///
    /// An id stays valid after a cancelled or failed run, because every
    /// runtime keeps the conversation as far as it got: claude and codex
    /// write their transcripts while they run, Ollama saves
    /// `logs/sessions/<id>.json` even when the run stops early. A run that
    /// failed before its `Session` event (for example a missing binary) has
    /// no id to keep. Always keep the latest `Session` id of a run, since a
    /// runtime may hand out a new one: codex starts a new thread when the old
    /// one is gone. If continuing fails anyway, for example because the user
    /// deleted the claude transcript, the run ends with `Finished(Err)` and
    /// the next run should start without a session.
    pub session: Option<String>,
    pub model: Option<String>,
    pub permissions: Permissions,
    /// How to start the MCP bridge for this agent. None runs without meerora tools.
    pub mcp: Option<McpBridge>,
    /// The CLI home and the sandbox of an agent run (D16, Q15, Q16). The
    /// daemon sets it for every agent run. None runs the CLIs with the
    /// user's own homes and without a sandbox, as the builder and tests do.
    pub isolation: Option<Isolation>,
}

/// Where an agent run keeps its CLI state and what it may see (D16, Q15, Q16).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Isolation {
    /// `<data dir>/runtimes`. The claude and codex CLIs of an agent get their
    /// own home below it, `<runtimes>/<cli>/<agent>/`. It lies outside the
    /// agent directory, so an agent cannot rewrite its own CLI configuration.
    pub runtimes: PathBuf,
    /// Runs the CLI, or each tool command of a model runtime, inside
    /// bubblewrap (see [`sandbox`]). Without it the CLI homes still apply.
    pub sandbox: bool,
    /// More paths the sandbox shows read-only, for example the space that the
    /// MCP bridge reads and `[sandbox] read` of `meerora.toml`. Missing paths
    /// are skipped.
    pub read: Vec<PathBuf>,
    /// Unix sockets the run may connect to, which is the agent's bridge socket
    /// of the daemon, for the meerora tools.
    pub sockets: Vec<PathBuf>,
    /// Workspace folders of the agent (`workspace` in `agent.toml`), already
    /// checked with `meerora_protocol::workspace::check`. The run may read and
    /// write them besides its directory. The sandbox keeps their `.git`
    /// read-only, and claude and codex get them with `--add-dir`.
    pub workspace: Vec<PathBuf>,
}

/// What a run may do besides reading its directory and calling the meerora
/// tools (D29).
///
/// `Permissions::default()` (all false) is read-only for every runtime,
/// which the builder relies on. Claude then gets no tool that writes:
/// `--tools` offers only Read, Glob and Grep, `--disallowedTools` denies
/// Edit, Write, MultiEdit, NotebookEdit and Bash, and `--permission-mode
/// dontAsk` refuses anything not allowed. Codex runs in its `read-only`
/// sandbox. Ollama gets no `write_file` and no `run_command`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Permissions {
    /// Edit files inside the agent directory.
    pub write: bool,
    /// Run commands inside the agent directory.
    pub shell: bool,
    /// Fetch web pages and search.
    pub web: bool,
}

impl Permissions {
    /// D29 defaults plus the `shell` and `web` capabilities.
    pub fn from_capabilities(caps: &meerora_protocol::CapabilitiesConfig) -> Self {
        Self { write: true, shell: caps.has("shell"), web: caps.has("web") }
    }
}

/// Command line of the MCP bridge, which speaks MCP on stdio.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpBridge {
    pub command: PathBuf,
    pub args: Vec<String>,
}

/// What a run reports, in order. The last event is always [`RunEvent::Finished`].
#[derive(Clone, Debug, PartialEq)]
pub enum RunEvent {
    /// Id to pass as [`RunRequest::session`] to continue this conversation.
    Session { id: String },
    /// A finished block of assistant text. Blocks of one run belong to one
    /// reply and are joined with a blank line.
    Text { text: String },
    /// A tool call, already in action-log form (D12). `tool` is one of
    /// `terminal.run`, `editor.patch`, `files.read`, `browser.open`,
    /// `browser.search` or `meerora.<name>`, and anything else as
    /// `tool.<name>`. `summary` is the command, path, URL or query, cut to
    /// [`actions::SUMMARY_CHARS`]. `target` is the full value when the
    /// summary had to be cut or flattened ([`actions::action_parts`]).
    Tool { tool: String, summary: String, target: Option<String> },
    /// Token use of the whole run, sent once right before `Finished` when
    /// the runtime reports it (also after an error, if any model call
    /// finished). Cached tokens are not included: `input_tokens` counts
    /// prompt tokens the model had to process anew, so a daily budget tracks
    /// new work rather than a resent system prompt. For claude that is
    /// `input_tokens + cache_creation_input_tokens` (cache reads left out),
    /// for codex `input_tokens - cached_input_tokens`, for Ollama
    /// `prompt_eval_count`, which skips the prefix Ollama kept from the last
    /// call. `output_tokens` includes reasoning tokens where they are billed.
    /// A cancelled run sends no `Usage`.
    Usage { input_tokens: u64, output_tokens: u64 },
    /// `Ok` carries the final answer text, `Err` a readable error.
    Finished { result: Result<String, String> },
}

/// A started run. Dropping the handle does not stop the run, [`RunHandle::cancel`] does.
pub struct RunHandle {
    pub events: Receiver<RunEvent>,
    cancel: Canceller,
}

impl RunHandle {
    pub fn new(events: Receiver<RunEvent>, cancel: Box<dyn Fn() + Send + Sync>) -> Self {
        Self { events, cancel: Canceller(Arc::from(cancel)) }
    }

    /// Stops the run. A `Finished` event with an error follows.
    pub fn cancel(&self) {
        self.cancel.cancel()
    }

    /// A cloneable handle that cancels this run from another thread, while
    /// the owner of the [`RunHandle`] keeps reading events.
    pub fn canceller(&self) -> Canceller {
        self.cancel.clone()
    }
}

/// Cancels one run, see [`RunHandle::canceller`]. Cancelling twice, or after
/// the run finished, does nothing.
#[derive(Clone)]
pub struct Canceller(Arc<dyn Fn() + Send + Sync>);

impl Canceller {
    /// Stops the run. A `Finished` event with the error "cancelled" follows
    /// right away, unless the run had already finished. Child processes get
    /// SIGTERM and, two seconds later, SIGKILL.
    pub fn cancel(&self) {
        (self.0)()
    }
}

impl std::fmt::Debug for Canceller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Canceller")
    }
}

pub trait Runtime: Send + Sync {
    /// For logs, for example "claude-cli".
    fn name(&self) -> &'static str;
    fn start(&self, request: RunRequest) -> anyhow::Result<RunHandle>;
}

/// The runtime for an agent's `[runtime]` table. `mock` has no runtime here
/// (the daemon plays mocks). `external` is an A2A client for agents from the
/// meerora index (D25, Q9), in `external.rs`.
///
/// Binaries come from `settings.paths` or PATH. A missing binary is not an
/// error here: the run reports it as `Finished(Err)`, for example "claude is
/// not installed (looked for claude on PATH)", so the agent panel can show it.
pub fn create(config: &RuntimeConfig, settings: &Settings) -> anyhow::Result<Box<dyn Runtime>> {
    match config.kind {
        RuntimeKind::ClaudeCli => Ok(Box::new(claude::ClaudeCli::new(config, settings))),
        RuntimeKind::CodexCli => Ok(Box::new(codex::CodexCli::new(config, settings))),
        RuntimeKind::Ollama => Ok(Box::new(ollama::Ollama::new(config, settings))),
        RuntimeKind::Api => anyhow::bail!("the api runtime is not available yet"),
        RuntimeKind::External => Ok(Box::new(external::External::new(config))),
        RuntimeKind::Mock => anyhow::bail!("mock agents are played by the daemon and have no runtime"),
    }
}
