//! The agent and app standard in the daemon (D30, docs/design/agent-apps.md).
//!
//! An agent uses apps through five tools of its MCP bridge: `apps_list`,
//! `app_read`, `app_screenshot`, `app_act` and `app_open`. The daemon checks
//! the capability of every call (D8):
//! - `apps.read` for `apps_list`, `app_read` and `app_screenshot`
//! - `apps.control` for `app_act` and `app_open` on built-in apps
//! - `apps.input` for `app_act` on real app windows (`window:*`)
//! - `apps.launch` for `app_open` of real apps
//!
//! These capabilities are checked per call, so a grant works at once and
//! needs no new run (`capabilities::per_call`). Every call is one `use` line
//! in the audit log, with `decision: refused` when the agent did not hold the
//! capability. A refused call tells the agent to ask with
//! `request_capability` first.
//!
//! The app holds the terminals, browsers and windows, so the daemon forwards
//! an allowed call to it as [`Event::AppRequest`] and answers the bridge
//! when the app replies with [`Command::AppReply`] (`meerora_protocol`).
//! Replies are matched by the request id.
//!
//! Which client is the app: a client says so with [`Command::ServeApps`]
//! after its hello. Requests go to the most recent such client that is
//! still connected. Bridges cannot send it, their sockets take only hello
//! and tool calls (server.rs), and the command line clients never send it.
//! Without such a client the call fails at once with "the meerora app is not
//! open". A request the app does not answer within [`APP_TIMEOUT`] fails as
//! well, and so does one whose app client leaves before it answers.
//!
//! A successful act or open carries a line for the agent's action log
//! (D12), which goes to every client as [`Event::Action`].

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use meerora_protocol::{AppCall, AppResult, Event, ToolCall};
use tracing::{info, warn};

use crate::daemon::{Daemon, Outgoing};
use crate::runs::cut;

/// How long the app may take to answer. The bridge waits a few seconds
/// longer (meerora-mcp), so this answer always reaches the model first.
pub const APP_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest summary in the action log, as for the other actions.
const SUMMARY_MAX: usize = 80;

/// The app clients and the requests that wait for their answer.
#[derive(Default)]
pub(crate) struct AppTools {
    /// Clients that sent `ServeApps`, oldest first. The last one gets the requests.
    hosts: Vec<u64>,
    pending: BTreeMap<String, Pending>,
    /// How long the app may take, [`APP_TIMEOUT`] except in tests.
    timeout: Option<Duration>,
}

/// A request the app has not answered yet.
struct Pending {
    /// The bridge client that called and its tool call id.
    client: u64,
    call: String,
    agent: String,
    tool: &'static str,
    /// The app client the request went to.
    host: u64,
    deadline: Instant,
}

impl AppTools {
    /// The app client that gets requests now.
    pub fn host(&self) -> Option<u64> {
        self.hosts.last().copied()
    }

    #[cfg(test)]
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = Some(timeout);
    }
}

/// What a capability lets an agent do, for the refusal.
fn describe(capability: &str) -> &'static str {
    match capability {
        "apps.read" => "read what your app windows show and take screenshots of them",
        "apps.control" => "type into your terminals, drive your browser pages and open built-in apps",
        "apps.input" => "click and type into real app windows",
        "apps.launch" => "start real desktop apps in your sandbox",
        _ => "use apps",
    }
}

impl Daemon {
    /// An app tool call of `agent` from the bridge `client` (D30).
    pub(crate) fn app_tool(&mut self, client: u64, call: String, agent: &str, tool: ToolCall) {
        let answer = |daemon: &mut Daemon, ok: bool, message: String| {
            daemon.out.push(Outgoing::To(client, Event::ToolResult { id: call.clone(), ok, message, image: None }));
        };
        let Some(request) = AppCall::from_tool(&tool) else {
            return answer(self, false, "this is not an app tool".into());
        };
        if !self.agents.contains_key(agent) {
            return answer(self, false, format!("unknown agent {agent:?}"));
        }
        let capability = request.capability();
        let name = request.tool();
        let note = match &request {
            AppCall::Act { app, action, .. } => format!("{name} {app} {action}"),
            other => other.app().map_or_else(|| name.to_string(), |app| format!("{name} {app}")),
        };
        let held = self.granted(agent);
        if !held.iter().any(|g| g == capability) {
            info!("{agent} called {note} without {capability}");
            self.caps.log_use(agent, capability, true, &note);
            let message = format!(
                "{name} needs the capability {capability}, which lets you {}. You do not hold it. Call request_capability with \
                 capability \"{capability}\" and a short reason first. Once the user allows it, call {name} again.",
                describe(capability)
            );
            return answer(self, false, message);
        }
        let Some(host) = self.app_tools.host() else {
            info!("{agent} called {note}, but no app serves app requests");
            return answer(self, false, format!("The meerora app is not open, so {name} cannot reach your apps. Try again later."));
        };
        self.caps.log_use(agent, capability, false, &note);
        let id = self.next_id("app");
        let timeout = self.app_tools.timeout.unwrap_or(APP_TIMEOUT);
        self.app_tools
            .pending
            .insert(id.clone(), Pending { client, call, agent: agent.to_string(), tool: name, host, deadline: Instant::now() + timeout });
        self.out.push(Outgoing::To(host, Event::AppRequest { id, agent: agent.to_string(), request, granted: held }));
    }

    /// `Command::ServeApps`: `client` holds the agent views from now on.
    pub(crate) fn serve_apps(&mut self, client: u64) {
        info!("client {client} serves app requests");
        self.app_tools.hosts.retain(|h| *h != client);
        self.app_tools.hosts.push(client);
    }

    /// A client left. When it was the app, its open requests fail.
    pub(crate) fn client_gone(&mut self, client: u64) {
        let before = self.app_tools.hosts.len();
        self.app_tools.hosts.retain(|h| *h != client);
        if before == self.app_tools.hosts.len() {
            return;
        }
        info!("client {client} no longer serves app requests");
        let lost: Vec<String> = self.app_tools.pending.iter().filter(|(_, p)| p.host == client).map(|(id, _)| id.clone()).collect();
        for id in lost {
            if let Some(p) = self.app_tools.pending.remove(&id) {
                let message = format!("The meerora app closed before it answered {}, so it is unclear whether it took effect.", p.tool);
                self.out.push(Outgoing::To(p.client, Event::ToolResult { id: p.call, ok: false, message, image: None }));
            }
        }
    }

    /// `Command::AppReply` from the app.
    pub(crate) fn app_reply(&mut self, id: String, result: AppResult) {
        let Some(p) = self.app_tools.pending.remove(&id) else {
            info!("app reply {id} is unknown or came too late");
            return;
        };
        if !result.ok {
            info!("{} of {} failed in the app ({})", p.tool, p.agent, result.text);
        }
        if let Some(log) = result.log.filter(|_| result.ok) {
            if self.agents.contains_key(&p.agent) {
                let summary = cut(log.summary.trim(), SUMMARY_MAX);
                let target = (summary != log.summary.trim()).then(|| log.summary.trim().to_string());
                self.emit(Event::Action { agent: p.agent.clone(), tool: log.tool, summary, target });
            } else {
                warn!("app reply {id} is for {}, which is gone", p.agent);
            }
        }
        self.out.push(Outgoing::To(p.client, Event::ToolResult { id: p.call, ok: result.ok, message: result.text, image: result.image }));
    }

    /// Fails the requests the app did not answer in time.
    pub(crate) fn tick_apps(&mut self, now: Instant) {
        let late: Vec<String> = self.app_tools.pending.iter().filter(|(_, p)| p.deadline <= now).map(|(id, _)| id.clone()).collect();
        let timeout = self.app_tools.timeout.unwrap_or(APP_TIMEOUT);
        for id in late {
            let Some(p) = self.app_tools.pending.remove(&id) else { continue };
            warn!("the app did not answer {} of {} in time ({id})", p.tool, p.agent);
            let message = format!(
                "The meerora app did not answer {} within {} seconds, so it is unclear whether it took effect.",
                p.tool,
                timeout.as_secs()
            );
            self.out.push(Outgoing::To(p.client, Event::ToolResult { id: p.call, ok: false, message, image: None }));
        }
    }

    /// How long until the next request times out.
    pub(crate) fn apps_wait(&self, now: Instant) -> Option<Duration> {
        self.app_tools.pending.values().map(|p| p.deadline.saturating_duration_since(now)).min()
    }
}
