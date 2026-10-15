//! meerora (phase 1 of D15): one shared space where every agent is a figure,
//! an agent view with a real terminal and chat, and the agent builder.
//!
//! ```text
//! meerora [--data-dir DIR] [--windowed | --session] [--no-daemon] [--no-spawn]
//! meerora snapshot OUT.png [--data-dir DIR] [--events FILE] [--warp SECONDS]
//!                  [--view space|builder|index[:N]|edit:ID|agent:ID|feed|profile[:review]|graveyard]
//!                  [--layout tiled|fullscreen] [--tab HANDLE] [--open HANDLE[=URL|PATH],...]
//!                  [--size WxH] [--scale F] [--builder-script FILE] [--builder-step N] [--turn DEG[,TILT]]
//!                  [--interview FILE] [--device ID] [--launcher] [--graveyard] [--retire ID] [--delete ID]
//!                  [--hover-stone ID] [--launch 'PROGRAM ARGS'] [--session] [--logout] [--rooms]
//!                  [--ask-device TEXT]
//!                  [--frames N [--fps F] [--turn-to DEG[,TILT]] [--zoom-to Z] [--inject FILE]] [--zoom-from Z] [--zoom-at X,Y]
//! ```

mod agent_apps;
mod app;
mod assets;
mod daemon;
mod desktop;
mod desktop_apps;
mod devices;
mod engine;
mod graveyard;
mod inhibit;
mod keymap;
mod nav;
mod portraits;
mod preview;
mod session;
mod snapshot;
mod theme;
mod ui;
mod world;

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};

const USAGE: &str = "\
meerora [--data-dir DIR] [--windowed | --session] [--no-daemon] [--no-spawn]
meerora snapshot OUT.png [--data-dir DIR] [--events FILE] [--warp SECONDS]
                 [--view space|builder|index[:N]|edit:ID|agent:ID|feed|profile[:review]|graveyard]
                 [--layout tiled|fullscreen] [--tab HANDLE] [--open HANDLE[=URL|PATH],...]
                 [--size WxH] [--scale F] [--builder-script FILE] [--builder-step N] [--turn DEG[,TILT]]
                 [--interview FILE] [--device ID] [--launcher] [--graveyard] [--retire ID] [--delete ID]
                 [--hover-stone ID] [--launch 'PROGRAM ARGS'] [--session] [--logout] [--rooms]
                 [--ask-device TEXT] [--commands QUERY [--answer TEXT]]
                 [--frames N [--fps F] [--turn-to DEG[,TILT]] [--zoom-to Z] [--inject FILE]]
                 [--zoom-from Z] [--zoom-at X,Y]

--frames N renders N frames OUT-0001.png, OUT-0002.png and so on instead of
one still, for videos (docs/video/). Each frame advances time by 1/F seconds
(--fps, 30 by default), so agents walk and rings turn. Over the frames the
space view eases from --turn to --turn-to and from --zoom-from to --zoom-to.
--zoom-at X,Y keeps that point of the screen, in points, in place while the
view zooms, as the scroll wheel does. --zoom-from also zooms a still.
--inject FILE applies events before given frames, one FRAME EVENT per line,
for example: 45 {\"type\":\"go_to\",\"agent\":\"ember\",\"station\":\"door\"}

--ask-device opens the dialog that asks an agent to build a device, with
TEXT in its field, which may be empty.

--commands opens the command box (Alt+Space) with QUERY typed and waits
for the scripts of the data dir. --answer shows TEXT as the finished quick
answer of a ?? query, where a backslash and n start a new line.

--session runs meerora as the login session: always fullscreen, always with
the desktop shortcuts, with the agent daemon and a Log out button that ends
the session. meerora-session starts it inside the cage compositor. In a
snapshot, --session shows the Log out button and --logout its question.

--launch starts a program on the desktop of the agent of --view agent:ID and
waits for its window, which shows as a tile.

--voice ready|recording|transcribing|speaking|downloading shows local voice
(D16) in that state, without a microphone or models. Without it a snapshot
shows no voice controls.

--tab selects an app instance by its handle: chat, editor:N, terminal:N,
browser:N, files:N, viewer:N, changes or activity, where a name without :N
is instance 1. --open opens more instances in the agent view first, for
example --open terminal:2,browser:2=https://example.org, and a browser
handle with =URL loads that page.

--open git:N=PATH opens the repository at PATH in that Git instance. --git
unified shows its diff unified, --git staged shows the first staged file,
and --git-message TEXT puts a commit message in the box, where a backslash
and n start the description.

Drag the space to turn the view, right drag or Shift drag to move it, scroll
to zoom and double-click to reset it.

Keys: F11 fullscreen, F3 frame stats, F2 tiled or fullscreen tiles,
Esc back to the space (when no terminal, editor, browser or app window has focus),
Alt+Tab and Alt+Shift+Tab switch the tabs of the agent view.";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,whisper_rs=warn".into()),
        )
        .init();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let dev_assets = meerora_protocol::assets_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets"));
    let mut data_dir = None;
    if let Some(i) = args.iter().position(|a| a == "--data-dir") {
        data_dir = Some(PathBuf::from(args.get(i + 1).context("--data-dir needs a value")?));
        args.drain(i..=i + 1);
    }
    let data_dir = data_dir.unwrap_or_else(|| meerora_protocol::resolve_data_dir(&dev_assets));
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);
    tracing::info!("data dir {}", data_dir.display());

    if args.first().map(String::as_str) == Some("snapshot") {
        return snapshot::run(snapshot_options(&args[1..], data_dir)?);
    }
    app::run(app_options(&args, data_dir)?)
}

/// The flags of the app itself. Window mode is the default, `--session` is
/// the login session (session.rs).
fn app_options(args: &[String], data_dir: PathBuf) -> anyhow::Result<app::Options> {
    let mut options = app::Options { data_dir, windowed: false, daemon: true, spawn_daemon: true, session: false };
    for a in args {
        match a.as_str() {
            "--windowed" => options.windowed = true,
            "--no-daemon" => options.daemon = false,
            "--no-spawn" => options.spawn_daemon = false,
            "--session" => options.session = true,
            other => bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    if options.session && options.windowed {
        bail!("--session always runs fullscreen, so it does not take --windowed");
    }
    if options.session && !options.daemon {
        bail!("--session needs the agent daemon, so it does not take --no-daemon");
    }
    Ok(options)
}

fn snapshot_options(args: &[String], data_dir: PathBuf) -> anyhow::Result<snapshot::Options> {
    let mut o = snapshot::Options {
        out: PathBuf::from(args.first().context("snapshot needs an output path")?),
        data_dir,
        events: None,
        builder_script: None,
        builder_step: 1,
        index_select: 0,
        interview: None,
        review: false,
        device: None,
        edit: None,
        turn: (0.0, 0.0),
        launcher: false,
        graveyard: false,
        retire: None,
        delete: None,
        hover_stone: None,
        rooms: false,
        ask_device: None,
        launch: None,
        session: None,
        voice: None,
        commands: None,
        answer: None,
        warp: 20.0,
        view: ui::View::Space,
        layout: ui::Layout::Tiled,
        tab: ui::AppTab::Terminal(1),
        open: Vec::new(),
        git: Vec::new(),
        git_message: None,
        width: 1440,
        height: 900,
        scale: 1.0,
        frames: 0,
        fps: 30.0,
        turn_to: None,
        zoom_from: 1.0,
        zoom_to: None,
        zoom_at: None,
        inject: None,
    };
    let mut it = args[1..].iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--events" => o.events = Some(PathBuf::from(value()?)),
            "--builder-script" => o.builder_script = Some(PathBuf::from(value()?)),
            "--builder-step" => o.builder_step = value()?.parse().context("--builder-step takes 1, 2 or 3")?,
            "--interview" => o.interview = Some(PathBuf::from(value()?)),
            "--device" => o.device = Some(value()?.clone()),
            "--warp" => o.warp = value()?.parse().context("--warp takes seconds")?,
            "--launcher" => o.launcher = true,
            "--graveyard" => o.graveyard = true,
            "--retire" => o.retire = Some(value()?.clone()),
            "--delete" => o.delete = Some(value()?.clone()),
            "--hover-stone" => o.hover_stone = Some(value()?.clone()),
            "--rooms" => o.rooms = true,
            "--ask-device" => o.ask_device = Some(value()?.clone()),
            "--launch" => o.launch = Some(value()?.split_whitespace().map(str::to_string).collect()),
            "--session" => o.session = Some(session::Session::default()),
            "--logout" => o.session = Some(session::Session::asking()),
            "--voice" => o.voice = Some(value()?.clone()),
            "--git" => o.git = value()?.split(',').map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect(),
            "--git-message" => o.git_message = Some(value()?.clone()),
            "--commands" => o.commands = Some(value()?.clone()),
            "--answer" => o.answer = Some(value()?.clone()),
            "--turn" => {
                let v = value()?;
                let (yaw, pitch) = v.split_once(',').unwrap_or((v.as_str(), "0"));
                o.turn = (yaw.parse().context("--turn takes degrees")?, pitch.parse().context("--turn takes degrees, then the tilt")?);
            }
            "--view" => {
                let v = value()?;
                o.view = match v.strip_prefix("agent:") {
                    Some(id) => ui::View::Agent(id.to_string()),
                    None if v == "space" => ui::View::Space,
                    None if v == "builder" => ui::View::Builder,
                    None if v == "feed" => ui::View::Feed,
                    None if v == "graveyard" => ui::View::Graveyard,
                    None if v == "profile" || v == "profile:review" => {
                        o.review = v == "profile:review";
                        ui::View::Profile
                    }
                    // The index screen with the results of --events, N selects one.
                    None if v == "index" || v.starts_with("index:") => {
                        o.index_select = v.strip_prefix("index:").map_or(Ok(0), str::parse).context("--view index:N takes a number")?;
                        ui::View::Index
                    }
                    None if v.starts_with("edit:") => {
                        o.edit = Some(v["edit:".len()..].to_string());
                        ui::View::Builder
                    }
                    None => {
                        bail!("--view is space, builder, index, index:N, edit:ID, agent:ID, feed, profile, profile:review or graveyard")
                    }
                }
            }
            "--layout" => {
                o.layout = match value()?.as_str() {
                    "tiled" => ui::Layout::Tiled,
                    "fullscreen" => ui::Layout::Fullscreen,
                    _ => bail!("--layout is tiled or fullscreen"),
                }
            }
            "--tab" => {
                o.tab = match value()?.as_str() {
                    "agent" => ui::AppTab::Chat,
                    v => {
                        v.parse().map_err(anyhow::Error::msg).context("--tab takes a handle such as chat, editor, terminal:2 or browser")?
                    }
                }
            }
            "--open" => {
                for item in value()?.split(',').filter(|i| !i.trim().is_empty()) {
                    let (handle, url) = item.split_once('=').map_or((item, None), |(h, u)| (h, Some(u.to_string())));
                    let tab: ui::AppTab = handle.parse().map_err(anyhow::Error::msg).context("--open takes handles such as terminal:2")?;
                    if url.is_some() && !matches!(tab, ui::AppTab::Browser(_) | ui::AppTab::Git(_)) {
                        bail!("--open takes =URL only for browser handles and =PATH for git handles, not {handle}");
                    }
                    o.open.push((tab, url));
                }
            }
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').context("--size is WIDTHxHEIGHT")?;
                o.width = w.parse()?;
                o.height = h.parse()?;
            }
            "--scale" => o.scale = value()?.parse()?,
            "--frames" => o.frames = value()?.parse().context("--frames takes a number of frames")?,
            "--fps" => o.fps = value()?.parse().context("--fps takes frames per second")?,
            "--turn-to" => o.turn_to = Some(pair(value()?, "--turn-to takes degrees, then the tilt")?),
            "--zoom-from" => o.zoom_from = value()?.parse().context("--zoom-from takes a factor such as 1.2")?,
            "--zoom-to" => o.zoom_to = Some(value()?.parse().context("--zoom-to takes a factor such as 1.2")?),
            "--zoom-at" => o.zoom_at = Some(pair(value()?, "--zoom-at takes X,Y in points")?),
            "--inject" => o.inject = Some(PathBuf::from(value()?)),
            other => bail!("unknown snapshot argument {other:?}\n\n{USAGE}"),
        }
    }
    if !o.fps.is_finite() || o.fps <= 0.0 {
        bail!("--fps must be more than 0");
    }
    Ok(o)
}

/// `A,B` as two numbers, `A` alone as `A,0`.
fn pair(v: &str, what: &str) -> anyhow::Result<(f32, f32)> {
    let (a, b) = v.split_once(',').unwrap_or((v, "0"));
    Ok((a.trim().parse().context(what.to_string())?, b.trim().parse().context(what.to_string())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<app::Options> {
        app_options(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), PathBuf::from("/data"))
    }

    #[test]
    fn window_mode_is_the_default() {
        let o = parse(&[]).unwrap();
        assert!(!o.session && !o.windowed && o.daemon && o.spawn_daemon);
        assert_eq!(o.data_dir, PathBuf::from("/data"));
        let o = parse(&["--windowed", "--no-spawn"]).unwrap();
        assert!(!o.session && o.windowed && o.daemon && !o.spawn_daemon);
    }

    #[test]
    fn session_mode_is_fullscreen_with_the_daemon() {
        let o = parse(&["--session"]).unwrap();
        assert!(o.session && !o.windowed && o.daemon && o.spawn_daemon);
        // A daemon that already runs, for example as a service, is fine.
        let o = parse(&["--session", "--no-spawn"]).unwrap();
        assert!(o.session && o.daemon && !o.spawn_daemon);
    }

    fn error(args: &[&str]) -> String {
        match parse(args) {
            Ok(_) => panic!("{args:?} should not parse"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn session_mode_refuses_a_window_and_no_daemon() {
        assert!(error(&["--session", "--windowed"]).contains("fullscreen"));
        assert!(error(&["--no-daemon", "--session"]).contains("daemon"));
        assert!(error(&["--sesion"]).contains("unknown argument"));
    }

    #[test]
    fn snapshots_show_the_session_parts() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let o = snapshot_options(&args(&["out.png", "--session"]), PathBuf::from("/data")).unwrap();
        assert!(o.session.is_some_and(|s| !s.asking));
        let o = snapshot_options(&args(&["out.png", "--logout"]), PathBuf::from("/data")).unwrap();
        assert!(o.session.is_some_and(|s| s.asking));
        assert!(snapshot_options(&args(&["out.png"]), PathBuf::from("/data")).unwrap().session.is_none());
    }
}
