# meerora

A desktop interface where every LLM agent is a small figure in one shared space that stands for your computer. The concept and all design decisions are in [docs/design/BRIEF.md](docs/design/BRIEF.md).

Milestone 2 is in (BRIEF section 11). Agents can run on a real LLM (the claude CLI, the Codex CLI or a local Ollama model), chat with you in the agent view, wake on a schedule, change their own figures and live in their own git-tracked directory. Agents set to `mock` keep playing the demo scenario.

## Install a release

```bash
curl -fsSL https://meerora.com/install.sh | sh
```

The script installs the prebuilt release for Linux on x86_64 into your home folder without root, adds a menu entry, creates `~/.local/share/meerora` with Claude and Rivet when you have none yet and starts meerora in a window. It never runs sudo. For missing packages such as bubblewrap it prints the install command. Until there is a release it offers to build from source. [website/README.md](website/README.md#install-script-and-releases) describes the script and how a release is made.

## Run it

```bash
cargo build
cargo run -p meerora -- --windowed
```

`cargo build` builds the app, the agent daemon `meerora-agentd` and the MCP bridge `meerora-mcp`. The app starts the daemon from the same folder when none is running. Leave out `--windowed` to start fullscreen.

Without further setup the app uses `assets/` from this repository, the demo data where every agent is a mock that plays the demo scenario. To get real agents, create your own data directory once:

```bash
cargo run -p meerora-agentd -- init     # creates ~/.local/share/meerora with Claude and Rivet on the claude CLI
cargo run -p meerora -- --windowed
```

A new data directory has the three rooms and two agents, both on the claude CLI with the model sonnet: Claude and Rivet, the helper (see Rivet, the helper below). The demo agents Ember, Moth, Quill and Wren and their scenario stay in `assets/`. Then click Claude or Rivet and type in the agent panel, or use the field at the top: `@claude what is in your memory?` or `@rivet set the command box key to Ctrl+Space`. Agents may only read and edit their own directory unless their `agent.toml` grants `shell` or `web` (D29). Settings such as the builder model and binary paths live in `meerora.toml` in the data directory.

Every real agent runs inside a bubblewrap sandbox (`/usr/bin/bwrap`) that shows the system read-only, its own directory and nothing else of your home (D16). The claude and Codex CLIs get their own home per agent below `runtimes/` in the data directory and share your login without copying it. An agent asks for a capability it lacks with a card in the space: Allow once, Always allow or Deny (D8). Always allow writes it into `agent.toml`, the lock button in the agent view's header revokes it, and every decision is logged in `audit.jsonl`. `meerora-agentd check` tells whether the sandbox works. Without bubblewrap, set `[sandbox] enabled = false` in `meerora.toml`. claude agents then need a login of their own, and the first run says how to make it.

| Key | Action |
| --- | --- |
| Click a figure or a sidebar row | Open the agent view |
| Drag the space | Turn the view around the space. The camera follows the pointer |
| Right drag or Shift drag, scroll | Move and zoom the view. Double-click resets it |
| Esc | Back to the space, unless the terminal, the editor, the browser or one of your apps has focus |
| F2 | Tiled or fullscreen tiles. In Files it renames the selected entry |
| + in the agent header | Open an app: Editor, Terminal, Browser, Files, Viewer, Changes, Git or Activity, or start one of your own apps under "Your apps". The cross on a tile closes it |
| Alt+Tab, Alt+Shift+Tab | Switch agents like apps on a desktop. A quick press goes back to the agent you viewed before. Hold Alt for a strip with every agent, press Tab to move on and let go of Alt to switch, Esc keeps the view. Also while one of your apps has the keyboard |
| Alt+\`, Alt+Shift+\` | Next or previous tab of the agent view |
| Drag the gap between tiles | Resize the tiles of the agent view |
| Drag a tile by its title | Move it: onto the middle of another tile to swap, onto an edge to place it there. Each agent keeps its layout in `layouts.json` |
| F3 | Frame rate, triangles and draw calls |
| F11 | Fullscreen window |
| Alt+Space | The command box: start apps, open agents, do math, ask an agent, search the web, run your own keywords. Also while one of your apps has the keyboard (see below) |
| Hold F8 | Push to talk. Speak and release, and the text lands in the focused field, ready to send (local voice, see below) |
| Right-click a sidebar row | Open, Edit or Move to graveyard. Rivet, the helper, has no Move to graveyard |
| Click a tombstone or "Graveyard (N)" | Open the graveyard with Bring back, Open folder and Delete for good |

The terminal tile is a real shell that starts in the agent's directory. "+ New agent" in the sidebar opens the builder (screen 3): describe the figure, an LLM writes and validates the script, and the preview plays its animations. Step 2 picks the model, what the agent may do and which windows its view shows (an agent that only talks can have just the chat).

The editor tile shows the agent's directory. Lines the agent changed since your last review are tinted in its colour, with Keep, Revert and Ask why (D12). The reviewed state is the ref `refs/meerora/reviewed` in the agent's git repository, so review needs a data directory where every agent is its own repository (`meerora-agentd init`). Ctrl+S saves. "Open in editor" in Changes shows the file as that change left it, in a read-only tab named like `run.py @ 1a2b3c4` with "Open current" for the file as it is now. The browser tile runs headless Chrome for each agent over CDP (D13), with its own profile in `<data dir>/browser/<agent id>/`. Set `MEERORA_CHROME` when Chrome is not on PATH as `google-chrome` or `chromium`.

Terminal, Browser, Editor, Files, Viewer and Git run as several instances in one agent view. "New" on their row in the "+" launcher opens another one beside the one you used last, titled "Terminal 2", "Browser 2" and so on, and a click on the row selects an open one. Every terminal is its own shell in the agent's directory, every browser its own page in the agent's one Chrome, and editors, Files and Viewers keep their own state. Closing one ends only its own shell or page. The agent's `browser.open` still goes to browser 1 and what it types to terminal 1. The head of a terminal tile has a shell strip: "+" opens a new shell as a tab of that tile, and once the agent has two terminals there is a tab for each, where a click shows that shell in this tile and "Own tile" in its context menu moves it back out. Every instance has a handle such as `chat`, `terminal:2`, `browser:1` or `window:7` (D30, [docs/design/agent-apps.md](docs/design/agent-apps.md)).

Files shows the agent's directory one folder at a time, folders first, sorted by name or time, without `.git` and `logs`. A dot in the agent's colour marks what it wrote since its last run. A double click or Enter opens text and code in the editor and images, Markdown, CSV, HTML and PDF in the Viewer. Files you drop on the window while Files shows are copied into the shown folder, under a new name like `report (2).pdf` when the name is taken (D25). Arrows, Enter, Backspace, F2 and Delete work as in a file manager. Delete moves to the trash with `gio trash` after a confirmation and never deletes for good. The Viewer shows the newest file in `artifacts/` until you open another one. It follows the file as it changes, and when the agent writes a new result the head says "New from <agent>" with a button to show it.

### Workspace folders

An agent can work on folders outside its own directory, such as a code repository. List them in its `agent.toml`, or in step 2 of the builder under "Workspace folders" with Add folder and Remove:

```toml
workspace = ["~/code/project", "/srv/site"]
```

The agent may read and write these folders besides its own directory. Its runs still start in its own directory, and the start of every run names the folders. The claude CLI gets each one with `--add-dir`, the Codex CLI with `--add-dir` when the agent may write, and Ollama's file tools take absolute paths inside them. The agent cannot change the list, because `agent.toml` is read-only in its sandbox.

The sandbox mounts each folder read-write and every `.git` in it read-only: the folder's own and those of the repositories one level below it. Such a nested repository is also mounted onto itself, so the agent cannot move it away and put one of its own in its place. The agent edits the code, but never the git configuration, the hooks or the index that your own git reads on the host. Committing and pushing stay with you, in the Git app or in a terminal.

The builder and the daemon refuse folders that would give an agent far more than a project, and the builder says why: `/`, your home folder or a folder that holds it, system folders such as `/usr`, `/etc` or `/var` (unless they lie in your home), folders with keys and settings such as `~/.ssh`, `~/.config` or `~/.local`, `/tmp` and `/home` themselves, and anything that holds meerora's data folder or lies inside it. The daemon checks the list again before every run and leaves a refused folder out with a warning in its log.

A `.git` is read-only only when it exists as a run starts. One that appears while an agent can write the folder, with `git init` for example, may carry a configuration the agent wrote, with a filter that runs a command for whoever uses git there. So the daemon keeps `workspace-trust.json` in the data directory. At the start of every run each `.git` that the sandbox protects and the record does not know yet is trusted when no agent has run in a folder that holds it, else untrusted. At the end of a run each `.git` that exists but was unknown is untrusted, because it was not protected during that run. The Git app runs your own git only on trusted repositories (below).

### The Git app

The Git app reviews the changes in a repository, lets you edit them right in the diff, and stages, commits and pushes them. Open it with "+" in the agent header. Every instance (`git:1`, `git:2`, ...) shows one repository. A new instance opens the repository it showed last, else the first workspace folder or the agent's own directory that is a repository and that no other instance shows. The repository name in the head opens the picker with the agent's workspace folders, its own directory, recent repositories (kept in `recent-repos.json` in the data directory) and a field for any path.

The head shows the branch, ahead and behind its upstream as `↑1 ↓0`, "Changed by Ember 2 min ago" when the agent's run patched files in this repository (D12), and Fetch and Push. The left column lists the staged, changed and untracked files with their counts. A dot in the agent's colour marks the files it patched, and `+` or `−` on a row stages or unstages that file. Stage all and Unstage all sit on the section heads. Below them is the commit box with a summary, a description and "Amend last commit", and Ctrl+Enter commits. An empty summary, nothing staged or unsaved edits keep Commit dim, and its tooltip says why.

The right side shows the selected file side by side or unified, with syntax colours and the changed words highlighted. Every hunk has Stage hunk and Discard among the changes, and Unstage hunk among the staged files. Discard asks first, because it restores the lines from the index and nothing goes to the trash. Untracked files are staged as a whole and never discarded here, Files moves them to the trash. The working side is editable: click a line and type. Enter splits the line, Backspace at its start joins it with the line above, Delete at its end joins the next one, the arrows move between lines and Esc stops editing. Save or Ctrl+S writes the file, and the diff follows. Unsaved edits block staging, committing, discarding and opening another file, with a note that offers Save and Drop edits.

Push runs `git push` in the background with its output in a panel under the diff, Stop ends it, and ahead and behind follow when it is done. A branch without an upstream offers `git push -u origin <branch>`. A rejected push, a failed login and a missing remote get a readable error. Fetch runs `git fetch`. git never runs on the UI thread. The app reads the status every two seconds while the tile shows and at once when it gets focus, without taking git's index lock.

Which git runs where:

| Repository | git | Why |
| --- | --- | --- |
| The agent's own directory | meerora's hardened git, as in the editor: no global or system configuration, no hooks, no fsmonitor, attributes only from the empty tree. Commits carry your name and email from your global git configuration | The agent writes this work tree, so nothing in it may run a command on your computer |
| Workspace folders and your other repositories | Your git with your configuration: name, email, signing, credential helpers, the SSH agent and hooks work as in a terminal. Only `core.fsmonitor` is off, because the app polls | Their `.git` is read-only for agents and your home is hidden from them, so they cannot write the configuration. `GIT_ATTR_SOURCE` stays unset, because ignoring `.gitattributes` would break Git LFS, line endings and binary markers, and an attribute can only name drivers that your configuration defines |
| An untrusted workspace repository, whose `.git` appeared while an agent could write there | Read-only: status and diff with the hardened git, without attribute files, submodules or any filter and diff driver its configuration names, and no git at all when the configuration includes other files. Nothing can be staged, edited, committed or pushed | The note says "This repository appeared while Ember could write here". Show .git/config shows its configuration and attribute files, and "Trust this repository" (with a confirmation) moves it to the trusted ones, after which your own git runs there. While an agent that can write it runs, the app runs no git there |
| A repository whose `.git` lies in an agent's directory, or deeper in a workspace folder than the sandbox protects | None, the app says why | An agent could have written its configuration |

Hooks of your repositories run as in your normal git. When `core.hooksPath` points into the work tree (husky, `.githooks`), agents can edit the hooks, so Commit asks first whenever a file there has uncommitted changes. Push and fetch run in a session of their own with `GIT_TERMINAL_PROMPT=0`, so neither git nor ssh can ask on a terminal. A credential helper or an SSH agent works, a password prompt does not. With the sandbox off (`[sandbox] enabled = false`) the app warns on workspace repositories, because agents could then change their git settings.

Agents use the Git app through the agent and app standard (D30) with `stage {path}`, `unstage {path}` and `commit {message}`. Their commits run without hooks, and pushing stays with you.

### In a window or as a login session

meerora runs as an app in a window of your desktop or as a login session of its own (D14, D15). The window is always there to try it, and every command above runs meerora in a window.

As a login session meerora runs fullscreen inside [cage](https://github.com/cage-kiosk/cage), a small kiosk compositor, and shows up next to Plasma on the login screen. The installer builds the release binaries as you, installs them to `/usr/local` and installs cage with dnf after asking. Running it again updates the installation.

```bash
scripts/install-session.sh --dry-run    # prints every step and changes nothing
sudo scripts/install-session.sh         # installs or updates the session
meerora-session --nested                # tries the installed session in a window, without logging out
```

| Installed path | Contents |
| --- | --- |
| `/usr/local/bin/meerora`, `meerora-agentd`, `meerora-mcp` | The release binaries |
| `/usr/local/bin/meerora-session` | The session launcher, a copy of `scripts/meerora-session` |
| `/usr/local/share/meerora/` | The shipped spaces, agents and scenarios, the licenses and the manifest for the uninstaller |
| `/usr/share/wayland-sessions/meerora.desktop` | The entry on the login screen |
| `/usr/local/lib/systemd/user/meerora-session.target` | Brings up `graphical-session.target` while the session runs |
| `/usr/local/share/xdg-desktop-portal/meerora-portals.conf` | Portals for `XDG_CURRENT_DESKTOP=meerora`, KDE's first and then GTK's |

To use the session, log out of Plasma, pick meerora in the session menu of the login screen and log in. `meerora-session` sets `XDG_CURRENT_DESKTOP=meerora` and the other session variables, takes the keyboard layout from `localectl` and creates `~/.local/share/meerora` with `meerora-agentd init` on the first start. It runs `cage -s` with itself inside, and that step hands the environment to systemd and D-Bus, starts PipeWire, the portal and KDE's polkit agent when they do not run, and runs `meerora --session`. SDDM writes the session's output to `~/.local/share/sddm/wayland-session.log`.

To get back to KDE, press Log out in the space header, or Ctrl+Alt+Delete while meerora has the keyboard, and confirm. meerora stops the daemon it started and quits, cage and the session end, and the login screen comes back, where you pick Plasma again. If meerora hangs, Ctrl+Alt+F3 opens a text console where you log in and run `pkill -u $USER cage`.

| | In a window | As the login session (`--session`) |
| --- | --- | --- |
| Fullscreen | `--windowed` or fullscreen, F11 switches | Always fullscreen and borderless |
| Desktop shortcuts | Alt+Tab, Super and the others reach meerora only while it has the keyboard and the desktop allows keyboard-shortcuts-inhibit (`[desktop] inhibit_shortcuts`). Shortcuts the desktop keeps stay with it | Every key reaches meerora. cage keeps only Ctrl+Alt+F1 to F12 for the consoles |
| Agent daemon | Started when none runs, `--no-daemon` and `--no-spawn` switch that off | Always used and started when none runs |
| Leaving | Close the window | Log out in the space header or Ctrl+Alt+Delete, then confirm |

The session has no lock screen, no notifications, no volume or brightness keys and no multi-monitor layout yet (Q6). cage spans all screens and runs them at scale 1. Windows that do not come from an agent's desktop, such as the polkit password prompt, portal file choosers and apps started through D-Bus activation, open over meerora as cage windows until they close.

To remove the session, run the uninstaller. It removes exactly what the installer added after asking, offers to remove cage when the installer installed it and leaves `~/.local/share/meerora` alone.

```bash
scripts/uninstall-session.sh --dry-run
sudo scripts/uninstall-session.sh
```

`bash scripts/tests/session-test.sh` tests the launcher with a fake cage and both scripts in a staging root (`--root`). `cargo test` runs it too.

### Your own apps in an agent view

Real apps, Wayland and X11, run as tiles in an agent's view (D15 phase 2). Every agent has its own desktop, a Wayland socket named `meerora-wl-<agent>` in `$XDG_RUNTIME_DIR`, served by a small compositor inside meerora (`crates/meerora-compositor`, Smithay, D16). Open the "+" in the agent header and pick an app under "Your apps". The list holds the installed apps from the `.desktop` files in `~/.local/share/applications` and every `applications` folder of `$XDG_DATA_DIRS`, the most recently used first, with a search field. The app starts in the agent's directory on the agent's desktop, and its window opens as a tile and becomes the selected tab. Apps you start yourself run as normal programs with your permissions. Apps an agent starts need `apps.launch` and run in its sandbox (see below).

A click into the window gives it the keyboard, and a thin blue frame shows that. From then on every key goes to the app, Esc, F2 and F11 included, as physical keys. A click anywhere else, on another tile, the rail or the header, gives the keyboard back to meerora, and so does Alt+Tab. Ctrl+V and Shift+Insert in an app paste the text of your clipboard, and text you copy in an app lands on your clipboard. The tile head shows the window's title, its cross asks the app to close, and the tile goes when the window is gone. Resizing a tile resizes the window. In fullscreen layout a window is a tab like any app.

Apps render in software (`GSK_RENDERER=cairo`, `LIBGL_ALWAYS_SOFTWARE=1`). They get the desktop's `WAYLAND_DISPLAY`, `DISPLAY` and session bus. Toolkits prefer Wayland (`GDK_BACKEND=wayland,x11`, `QT_QPA_PLATFORM=wayland;xcb`), and X11-only apps such as xmessage run through Xwayland. Each desktop gets its own X display, started with the first app launched on it, because meerora cannot tell an X11-only app before it runs and the X socket takes connections before Xwayland is ready. X11 windows become tiles like Wayland windows, with their title, resizing, focus, keys and menus. The compositor API can force a toolkit app onto X11 (`LaunchOptions { x11: X11::Force, .. }`) for apps that misbehave on Wayland. Apps with client side decorations, such as GTK apps and weston-terminal, keep their own title bar inside the tile.

Every desktop also has a private D-Bus session bus, so single-instance apps such as Files, Dolphin or Text Editor find the instance on the agent's desktop and open their window in the tile instead of handing the launch to an instance on your desktop. The bus starts no services by itself, because a second dconf, gvfs, keyring or document portal would write the same files as the ones of your session. Apps on it therefore lose what your session bus offers: desktop notifications, portals (the file chooser of sandboxed apps, screenshots, opening links), saving settings through GSettings (they read your settings, but changes are dropped), gvfs locations such as `trash:` and network shares, the secret service (saved passwords) and accessibility. Without `dbus-daemon` your apps use your session bus as before, and an app that still hands its launch to an instance outside meerora gets a toast.

The pointer takes the shape the app asks for, a named cursor through `wp_cursor_shape_v1` or the app's own cursor image. With a fractional display scale such as 1.25, apps that support `wp_fractional_scale_v1` render at that scale and stay sharp, other Wayland apps render at the next integer scale and X11 apps at 1. Text you select in one app pastes into another with a middle click (primary selection), also between Wayland and X11 apps. Only regions that changed are composed and uploaded. Apps keep running while the meerora window is hidden, and a window whose tile is hidden still gets a frame callback once a second.

Keys go to apps with the `us` layout unless `meerora.toml` says otherwise. While the meerora window has the keyboard, it also asks the desktop for Alt+Tab, Super and the other desktop shortcuts (Wayland keyboard-shortcuts-inhibit, D15 phase 1). GNOME asks once whether to allow that. Under X11, or on a desktop without the protocol, shortcuts stay with the desktop. Both live in `meerora.toml`:

```toml
[desktop]
keyboard_layout = "de"        # XKB layout for your apps, "us" when unset
keyboard_variant = "nodeadkeys"
inhibit_shortcuts = false     # leave Alt+Tab, Super and the others to the desktop
```

### Apps an agent starts

Agents start apps with the tool `app_open` (see below). It needs the capability `apps.launch`, which the agent asks for like `shell` or `web` (D8). Such an app runs in bubblewrap with the sandbox code of agent runs (D16): the system read-only, the agent directory read-write as its home and working directory, a private `/tmp`, `/dev` and `/proc`, an allowlist of environment variables, no network unless the caller allows it, and nothing of your home. It talks to the agent realm of the desktop, which has a Wayland socket, an X display and a session bus of its own, so an agent's app can neither read the keys of your X11 apps nor talk to your apps over D-Bus. App settings and caches stay in the sandbox's `/tmp` and go with the app. Its tile head says `<AGENT> · SANDBOX`. The API is `Compositor::launch_sandboxed(agent, agent_dir, program, args, &SandboxOptions)` in `crates/meerora-compositor`.

### How agents use your apps

Agents see and use the windows of their view through five meerora tools (D30, [docs/design/agent-apps.md](docs/design/agent-apps.md)). Every window is an app instance with a handle such as `terminal:1`, `browser:2`, `editor:1` or `window:7`.

| Tool | What it does | Capability |
| --- | --- | --- |
| `apps_list` | the open instances with handle, kind, title, focus and who controls them | `apps.read` |
| `app_read` | what an instance shows as text, with the elements to act on, each with a ref | `apps.read` |
| `app_screenshot` | a PNG of a browser page or a real app window, which reaches the model as an image | `apps.read` |
| `app_act` | one action, for example `run {command}` in a terminal, `click {ref}` in a browser or `type {text}` in a window | `apps.control`, `apps.input` for windows |
| `app_open` | a new terminal, browser, editor, Files or viewer, or a real app in the agent's sandbox | `apps.control`, `apps.launch` for real apps |

An agent asks for these capabilities with `request_capability` like for `shell` or `web`, and they work as soon as you allow them. Keep in mind that the terminal tile runs your own shell, not the sandbox, so `apps.control` lets an agent run commands as you there. The access card under the padlock says so.

Everything an agent does shows in its action log as `terminal.run cargo test`, `browser.click Sign in` or `window.type ...`, and the tile head says "CLAUDE IS TYPING" while it acts. Take over on a tile head stops every agent action on that tile until you hand it back. Real app windows got Take over and show the agent's cursor where it clicked. Every call goes to `audit.jsonl` as a `use` line.

The daemon checks each call and forwards it to the app (`Event::AppRequest`, answered with `Command::AppReply`), because the app holds the terminals, browsers and windows. When meerora is not open, the tools say so. To try it from the command line, give an agent `apps.read` and `apps.control` in its `agent.toml` and run `meerora-agentd say claude "Run echo hi in terminal:1 with app_act"` while meerora is open.

### Voice

Talking to agents and hearing their replies runs on this machine (D16). Speech to text is whisper.cpp with the multilingual base model, so English and German both work. Text to speech is Piper with one English and one German voice. Hold F8, speak and release. The text lands in the field that had focus, the talk field in the space or the chat input in an agent view, and waits there until you send it. Holding the microphone button in either field does the same. While you speak, a small indicator next to the field shows the level, then "Transcribing". F8 was picked because meerora itself uses F2, F3 and F11. While voice is on, the terminal and browser tiles no longer get F8, so set another `key` if a program there needs it. The key works while meerora has the keyboard, not while one of your own apps has it.

The speaker button in the chat input reads that agent's replies aloud. The voice follows the language of the reply, German or English. A pill at the bottom shows who is speaking, and its Stop button ends the speech and drops what was queued. Code blocks and link targets are not read.

The models download on first use, when you first press F8 or when the first reply should be spoken, into `~/.cache/meerora/voice/`. The voice controls stay hidden until they are there. Every file is pinned to a commit or release and checked against its size and SHA-256 before it is kept:

| File | From | Size |
| --- | --- | --- |
| whisper `ggml-base.bin` | huggingface.co/ggerganov/whisper.cpp | 148 MB |
| Piper 2023.11.14-2, Linux x86_64 | github.com/rhasspy/piper | 26 MB, 55 MB unpacked |
| `en_US-lessac-medium` | huggingface.co/rhasspy/piper-voices | 63 MB |
| `de_DE-thorsten-medium` | huggingface.co/rhasspy/piper-voices | 63 MB |

That is about 300 MB to download and 330 MB on disk. A download that would leave less than 1 GB free is refused. Recording uses `pw-record` and playback `pw-play` from PipeWire. whisper.cpp is built from source by the whisper-rs crate during `cargo build`, which needs cmake, a C++ compiler and libclang.

```toml
[voice]
enabled = true             # false hides every voice control and downloads nothing
key = "F8"                 # the key to hold, by its egui name
language = "auto"          # what you speak for whisper: auto, en, de, ...
speak_replies = false      # read every agent's replies aloud

[voice.voices]             # Piper voice per language of a reply
en = "en_US-lessac-medium"
de = "de_DE-thorsten-medium"

[voice.agents.claude]      # per agent
speak_replies = true
voice = "en_US-lessac-medium"   # always this voice
```

The speaker button wins over `[voice.agents]`, which wins over `speak_replies`. Its choice is kept in `voice.json` in the data directory, because the app changes nothing in `meerora.toml` except the `[space] room` line of the room picker (see Rooms below). Other Piper voices from the piper-voices repository work by name, without a pinned checksum. To turn voice off, set `enabled = false` under `[voice]`. To free the disk space, delete `~/.cache/meerora/voice/`.

The same from the command line, also to try the microphone:

```bash
cargo run -p meerora-voice -- status                  # what is downloaded
cargo run -p meerora-voice -- fetch                   # download everything now
cargo run -p meerora-voice -- listen --seconds 5      # record, then print the text
cargo run -p meerora-voice -- say "Hallo, ich bin Claude."
cargo run -p meerora-voice -- say --out hello.wav "Hello there."
cargo run -p meerora-voice -- transcribe hello.wav
```

### The command box

Alt+Space opens a field with results over any view, like Alfred on the Mac. Esc, Alt+Space again or a click outside closes it. The results follow every key you type. Up and Down select, Enter runs the selected row, Shift+Enter runs its alternative when the footer names one, and Tab completes a keyword. Every row says under its title what Enter does. The key reaches meerora like Alt+Tab, through keyboard-shortcuts-inhibit in a window and always in the session. In Plasma, Alt+Space also opens KRunner. While meerora has the keyboard it goes to meerora, elsewhere KRunner keeps it.

| Type | Enter does |
| --- | --- |
| An app's name, like `fire` or `term` | Installed apps start as real apps on an agent's desktop, built-in apps open in that agent's view. Matching is fuzzy, and what you used recently comes first |
| An agent's name | Opens its view. Shift+Enter opens its chat |
| `@name message` | Sends the message, like the talk field. `@name` alone opens the view |
| `2+2*3`, `sqrt(2)^3`, `0x1f + 0b101`, `200 + 10%`, `10% of 200`, `5!`, `5 km in miles`, `100 c in f` | Shows the result while you type and copies it. Shift+Enter puts it into the field |
| `? question` or `ask question` | Sends the question to the assistant agent and opens its chat, where the conversation goes on |
| `?? question` | Asks the assistant for a quick answer in the box, through the daemon, without opening a view. Enter asks, the answer grows as it arrives, and then Enter copies it. Esc stops it |
| `g words` | Searches the web in the browser of the target agent |
| A URL, like `docs.rs/egui` | Opens it in the target agent's browser |
| `space`, `feed`, `graveyard`, `room`, `new agent` | Goes there. `Room: Office` switches the room at once |
| `settings` | "Command box settings" opens `commands.toml` in a text editor on the target agent's desktop |
| A keyword of yours | Runs it, see below |

Apps start on the desktop of the agent whose view is open. In the space they start on the desktop of the agent you viewed last, and the pill on the right of the field names it. Ctrl+Tab and Ctrl+Shift+Tab pick another agent. Enter on an installed app also opens that agent's view, so its window shows, and Shift+Enter starts it and stays where you are. Web pages, keywords and scripts use the same agent.

The calculator knows `+ - * / ^ mod`, `%` as percent (and as `mod` between two numbers), `!`, hex, binary and octal numbers, `sqrt cbrt abs ln log log2 exp sin cos tan asin acos atan floor ceil round min max pow hypot`, `pi`, `e` and `tau`, and units of length, mass, time, data, volume, speed, area, energy and temperature with `in`, `to` or `as`. It is a small parser of its own, so nothing typed into the box runs as a script.

Three things are set in `meerora.toml`:

```toml
[commands]
key = "Alt+Space"                               # also Meta+Space, Ctrl+Space, Alt+F1, F12. Meta, Ctrl and Alt combine with a key
assistant = "claude"                            # the agent ?, ask and ?? go to. It must exist
search = "https://duckduckgo.com/?q={query}"    # what g opens
```

Your own keywords live in `commands.toml` in the data directory. The first time the box opens, meerora writes a commented example there if the file is missing, and never replaces one. Each `[[command]]` has a keyword, a title and one action. Type the keyword, a space and your text, and `{query}` stands for that text. URLs get it percent-encoded, launch arguments stay separate words without a shell.

```toml
[[command]]
keyword = "w"
title = "Search Wikipedia"
open_url = "https://en.wikipedia.org/w/index.php?search={query}"   # in the target agent's browser

[[command]]
keyword = "calc"
title = "Start the calculator"
launch = ["gnome-calculator"]          # a real app on the target agent's desktop

[[command]]
keyword = "review"
title = "Ask Ember for a review"
say = "Review {query} and tell me what to fix."
agent = "ember"                        # the assistant when left out

[[command]]
keyword = "ed"
title = "Open the editor"
open_app = "editor"                    # chat, editor, terminal, browser, files, viewer, changes, activity, or terminal:2

[[command]]
keyword = "shrug"
title = "Copy a shrug"
copy = "¯\\_(ツ)_/¯"                   # notify = "..." shows a note instead
```

`subtitle` adds a line under the title. A keyword without `{query}` runs at once, one with it waits for your text. A broken entry shows as a red row at the top of the empty box, and the others still work. The box reads the file again when it changes.

Commands that compute their results are JavaScript in `commands/` of the data directory, one command per file:

```js
export default command({
  keyword: "temp",
  title: "Convert temperatures",
  run(query) {
    const c = Number(query);
    if (!Number.isFinite(c)) return [];
    const f = c * 9 / 5 + 32;
    return [{ title: `${c} °C = ${f} °F`, action: copy(f), alt: copy(`${f} °F`) }];
  },
});
```

`run` gets the text after the keyword and returns rows with a title, an optional subtitle and glyph, and an action for Enter and Shift+Enter: `openUrl`, `launch`, `say`, `openApp`, `copy`, `notify` or `calc`. Actions are data. The script itself runs in the sandbox of figure scripts, without files, network or processes, and meerora carries out an action only when you pick its row, which always says what it runs (D8). Each script keeps one QuickJS context on a thread of its own between keystrokes, with 200 ms and 16 MiB per call. A script that loops forever is stopped, and errors show as a row. Saving a script loads it again. Two examples are copied into `commands/` on the first open: `units.js` (`temp 21`) and `github.js` (`gh owner/repo`, `gh owner/repo#12`, `gh words`). The whole API is in `crates/meerora-script/API.md`.

The box remembers what you ran in `commands-recent.json` of the data directory, and it reads the launcher's `recent-apps.json`, so the empty box starts with what you used last.

### Devices

Objects in the space are live controls (D7). The paper plane on the window sill follows airplane mode both ways: it takes off and circles the room while airplane mode is on, however you switched it. The leaves of the tree show how much memory is free (full, thin from below 30 %, bare from below 10 %), the hands of the clock show the time, the mailbox flag goes up while Wren holds up a letter, and the noticeboard opens the feed. The screen on the back wall draws a live graph of the CPU in use. Its card switches it to free memory, the disk in use or the network traffic, and the daemon keeps the choice in `devices/settings.toml` of the room. Click a device for its card with its state, who owns it, its capabilities and its actions. The first click on Toggle airplane mode brings the approval card, because the plane needs `network.airplane_mode.write` (D8). Always allow keeps the grant in `devices/grants.toml` of the room and puts a padlock on the plane. Every request, decision and use is logged in `audit.jsonl`. The daemon switches the radios with `rfkill block all` or `rfkill unblock all`, and falls back to `nmcli radio all off` or `on`.

The devices of the space are scripts in `devices/` of the room (see Rooms below). Agents make their own by writing `devices/<name>.js` into their directory, and the daemon picks them up within a few seconds. Such a device may only bind and use what its agent holds, so it does not load otherwise and the agent reads why in its chat. The script API is in `crates/meerora-script/API.md`.

The sidebar lists the devices of the space under the agents (a click on DEVICES folds the list, and it stays folded after a restart), with their state, a padlock for their capabilities and the maker of an agent's device. A click opens the device card. "Ask an agent to build a device" at the end of the list asks for one line about the device and an agent to build it, by default the agent you viewed last. meerora sends the agent the request in its chat and opens its view. Mock agents only play the demo, and external agents have no files here, so they cannot build devices. The daemon keeps a copy of the script API in `docs/API.md` of the data directory, which agents can read inside their sandbox.

To change an agent, open it and press "Edit agent". The same three steps open with its current look and settings, and "Save changes" writes them back to its directory. You can also edit `agent.toml` and `figure.js` in the agent's directory by hand. Scripts reload while the app runs. Save an agent's `figure.js` or the `space.js` of the current room (`spaces/<room>/` of the data directory, see Rooms below) and the figure or the space rebuilds. A broken script keeps the previous version and shows an error.

### Rooms

The space comes in three rooms (D6): the workshop with its garden, a space station with a hydroponics bay and an office with a lounge. Each has the same stations, so every agent's home and every tool station works in every room, and each has its own plane, clock, system graph, memory plant, mailbox and noticeboard. Click the space card at the top of the sidebar to pick one. The picker writes the choice as `[space] room` into `meerora.toml` and leaves the rest of the file as it is.

```toml
[space]
room = "office"        # workshop, space-station, office or custom
```

The rooms ship as templates in `assets/spaces/<room>/` with `space.js` and `devices/`. A room lives in `spaces/<room>/` of the data directory, together with the grants and settings of its devices. The first time you pick a room that the data directory lacks, meerora copies its template there, and from then on the copy is yours to edit. Switching only changes the setting, so it never touches the files of another room. A `space/` folder from before rooms stays as it is and is the room "Custom". Without the setting, a data directory with `space/` keeps using it, and one without uses the workshop.

To run the daemon on its own and send it events by hand:

```bash
cargo run -p meerora-agentd                        # terminal 1
cargo run -p meerora -- --windowed --no-spawn      # terminal 2
cargo run -q -p meerora-agentd -- send '{"type":"go_to","agent":"ember","station":"door"}'
cargo run -q -p meerora-agentd -- say claude "hello"   # prints the run's events
cargo run -q -p meerora-agentd -- check            # validates the scenario, agents and runtimes
```

### Rivet, the helper

Rivet, the small robot, is meerora's helper and always there. It is a real agent (`role = "helper"` in its `agent.toml`, with the double violet ring of a system agent) that explains meerora and helps you set up meerora and your computer. Ask it how something works, or ask it to change something: `@rivet give Claude the web`, `@rivet switch to the office`, `@rivet add a keyword that searches Wikipedia`. It looks things up in this README, which is built into the daemon, with `config_read readme`.

When the daemon starts and no agent has `role = "helper"`, it creates `agents/rivet/` from the template in `assets/templates/helper/` (built into the binary), so the helper comes back even when its directory is gone. Rivet cannot be retired: the sidebar menu and the builder have no Move to graveyard for it, and the daemon refuses `meerora-agentd retire rivet` with a message. Only one agent is the helper.

Only the helper has two more tools:

| Tool | What it does |
| --- | --- |
| `config_read {target}` | meerora's configuration as text: `meerora.toml`, `agents` (every agent with its runtime, role and capabilities), `agent:<id>` (its `agent.toml`), `rooms` (the current room, its stations and devices), `commands` (`commands.toml`), `audit` (the last 50 lines of `audit.jsonl`), `check` (what `meerora-agentd check` prints), `readme` (the sections of this README) or `readme:<words>` (the sections that match). The index token in `index-account.json` and the CLI homes in `runtimes/` never leave, and a token in a text is hidden |
| `config_change {target, content, reason}` | Proposes the whole new content of `meerora.toml`, `commands.toml`, `agent:<id>` or `new_agent:<id>` (an `agent.toml` and an optional figure script in `figure`). The call waits until you decide |

The daemon checks a proposal the way meerora reads the file: the TOML, the settings and their `[commands]`, the stations and schedules of an agent, capability names, workspace folders and roles. It refuses what would not work with an error that Rivet reads and fixes. A proposal that passes shows as a card with the diff, Rivet's reason, Apply and Discard. There is no "always" for configuration changes. Lines that change what an agent may do or how it runs (`granted`, `system`, `workspace`, `role`, the runtime, and `[sandbox]`, `[paths]` and `[builder]` of `meerora.toml`) are highlighted, because granting a capability this way is allowed only because you read the diff. Only Apply writes the file, keeping its leading comment lines when the new content has none, and meerora uses it at once. A file that changed since the proposal is not overwritten. Every proposal, decision and closed proposal is a line in `audit.jsonl`. A proposal closes without a write when Rivet's run is cancelled or after ten minutes without a decision.

For your operating system, such as a package, a service or the network, Rivet explains what it wants to run and why, then types each command into your terminal tile with `app_act` and reads the result, so you see every command. That terminal runs as you, so Rivet asks for `apps.control` and `apps.read` when it first needs them, and you can take over the tile at any time.

To render the card without a window:

```bash
cargo run -p meerora -- snapshot out.png --events assets/scenarios/config-approval.jsonl
```

A data directory from before the helper has the mock Rivet of the demo in `agents/rivet/`. The daemon never overwrites it and runs without a helper until you move it aside, for example into the graveyard with `meerora-agentd retire rivet` while the daemon runs, or by moving `agents/rivet` out of the data directory while it does not. The next start of the daemon creates the helper.

### The graveyard

An agent you no longer need moves to the graveyard, and it can come back. Rivet, the helper, always stays. "Move to graveyard" sits at the bottom of the builder in edit mode and in the context menu of the agent's sidebar row, and both ask first. The daemon stops its run, its queue and its schedule, takes it out of the space and moves its directory to `graveyard/<id>/` in the data directory, with its git history, memory and logs and a `retired.json`. Its CLI homes stay, so it continues its conversations when it comes back under the same id. Before it leaves, its figure walks to the graveyard, and it leaves the space when it arrives there, after 15 s at the latest. A small tombstone with a band in the agent's colour stands for it on the fenced plot behind the tree. A click on a tombstone or on "Graveyard (N)" in the sidebar opens the list. Bring back moves the directory home, the agent returns to its home station and its schedule resumes. Delete for good moves the folder to the system trash with `gio trash` after a second question that names it. meerora never deletes it for good. The space script declares the plot with `graveyard: { station, gap, width, depth }`. A space without it, such as an older `space.js`, shows no tombstones, and the list still works. The same from the command line:

```bash
cargo run -q -p meerora-agentd -- retire moth
cargo run -q -p meerora-agentd -- graveyard          # the retired agents with their ids in the graveyard
cargo run -q -p meerora-agentd -- restore moth
```

### External agents

Companies, institutions and people publish agents in the meerora index (D25). Until a public index exists, run a local one with three demo agents and a demo A2A server that answers for them:

```bash
cargo run -p meerora-index -- demo-agent &                                # A2A 1.0 on 127.0.0.1:7879
cargo run -p meerora-index -- serve --demo --allow-http-localhost &       # the index on 127.0.0.1:7878
```

"Get an external agent" under "+ New agent" in the sidebar searches the index, shows a live preview of each figure and imports the agent. It arrives at the door with an "external" badge, and what you type to it goes to its owner's endpoint. It gets no files and no tools. The same from the command line:

```bash
cargo run -q -p meerora-agentd -- search "data residency"
cargo run -q -p meerora-agentd -- import meridian.example/mira
cargo run -q -p meerora-agentd -- say mira "What do seats cost?"
```

The daemon asks the index at `[index] url` in `meerora.toml`, `http://127.0.0.1:7878` when unset. `meerora-index serve --help` lists the API.

An organisation registers its agent with `POST /v1/manifests` (a JSON manifest of at most 16 KB). The manifest counts as verified when `https://<domain>/.well-known/meerora.json` lists its id together with the SHA-256 of the exact bytes that were posted, so `sha256sum mira.json` gives the value:

```json
{ "meerora": "0.1", "manifests": [{ "id": "meridian.example/mira", "sha256": "<sha256sum of mira.json>" }] }
```

To change a verified manifest, list the hash of the new file first, then post it. The endpoint, figure, picture and agent card URLs of a verified manifest must be on the owner's domain or below it. The index fetches the well-known file only from public addresses, without redirects, and refuses IP addresses and internal names. The first upload of an id that does not verify answers with an `edit_secret`, the only time it is shown. Later unverified uploads of that id must send it as `Authorization: Bearer <edit secret>`, and an unverified upload never replaces a verified manifest. People cannot post here, their agent comes from their account (below). With `--demo` the index serves the well-known file for the `.example` demo domains with the hash of the demo manifest, so Meridian verifies. `--allow-http-localhost` is for tests and the demo only, a public index sets `--public-url https://...` instead.

### Your personal agent and the feed

Your own agent represents you in the index (D26). The Feed button in the space header opens the feed. Press "Set up your personal agent" there: the builder makes its figure and writes `agents/me/` with `role = "personal"` in its `agent.toml`. An interview follows, in which the agent's model asks one question at a time and fills your profile. In the review you set every field to public, friends or private, add photos (a path, or drop the files on the window), create your account and publish. Private fields stay in `agents/me/config/profile.json` and never reach the index. The token of your account stays in `index-account.json` in the data directory, readable only by you.

Once published, people find you by name in "Get an external agent". Follow and Befriend sit in its header. A follow shows public posts, a friendship needs both sides and also shows friends fields and friends posts (D27). With `--demo` the index has Alex Example, who accepts friend requests at once, and posts from Mira, Tess and Alex. Friends and the agents you follow appear in the feed only, never as figures in your space (Q12).

Your personal agent drafts posts about you when you ask it, with its `draft_post` tool. Every draft waits in the feed with Approve, Edit and Discard, and nothing reaches the index before you approve it. "Add the news agent" in the feed writes `agents/news/` from a template (D28): it wakes at 07:00, reads the sources you list in the feed and your feed, and pins a digest on top. Your More, Less and Open clicks go to its `memory/reactions.jsonl`, and untouched items count as skipped when the next digest arrives.

The same from the command line:

```bash
cargo run -q -p meerora-agentd -- social account robin           # create the account
cargo run -q -p meerora-agentd -- social publish profile.json    # save and publish a profile
cargo run -q -p meerora-agentd -- social follow meridian.example/mira
cargo run -q -p meerora-agentd -- social befriend alex
cargo run -q -p meerora-agentd -- social feed
cargo run -q -p meerora-agentd -- social drafts                  # then: social approve ID
cargo run -q -p meerora-agentd -- social news                    # add the news agent
```

To render the whole UI to a PNG without a window:

```bash
cargo run -p meerora -- snapshot out.png --events assets/scenarios/screen1.jsonl
cargo run -p meerora -- snapshot out.png --events assets/scenarios/screen1.jsonl --view agent:ember --layout fullscreen
cargo run -p meerora -- snapshot out.png --view builder --builder-script assets/agents/wren/figure.js
cargo run -p meerora -- snapshot out.png --events feed.jsonl --view feed            # also profile, profile:review
cargo run -p meerora -- snapshot out.png --events assets/scenarios/devices.jsonl --device airplane      # a device card
cargo run -p meerora -- snapshot out.png --events assets/scenarios/devices-approval.jsonl               # its approval card
cargo run -p meerora -- snapshot out.png --events assets/scenarios/config-approval.jsonl                # a configuration change of the helper
cargo run -p meerora -- snapshot out.png --data-dir DIR --graveyard --view graveyard   # also --retire ID, --delete ID, --hover-stone ID
cargo run -p meerora -- snapshot out.png --events assets/scenarios/devices.jsonl --device graph       # the system graph with its card
cargo run -p meerora -- snapshot out.png --rooms                                                     # the room picker
cargo run -p meerora -- snapshot out.png --events assets/scenarios/screen1.jsonl --ask-device "a lamp that glows while CI runs"   # the dialog of the devices list
cargo run -p meerora -- snapshot out.png --data-dir DIR --view agent:wren --launch "weston-terminal --shell=/bin/sh"   # waits for the window
cargo run -p meerora -- snapshot out.png --events assets/scenarios/screen1.jsonl --view agent:claude --tab chat --voice recording   # also ready, transcribing, speaking, downloading
cargo run -p meerora -- snapshot out.png --data-dir DIR --commands "te"                                  # the command box with this query
cargo run -p meerora -- snapshot out.png --data-dir DIR --commands "?? why is the sky blue" --answer "Rayleigh scattering."   # with a quick answer
```

`--launch` writes `layouts.json` into the data directory, so give it a copy such as `cp -r assets /tmp/data`. A `;` between words starts several apps, for example `--launch "env GDK_BACKEND=x11 gnome-calculator ; weston-terminal --shell=/bin/sh"` for an X11 app next to a Wayland app, and the snapshot waits for all their windows.

Screen 2 with a reviewed change in the editor and a page in the browser needs a data directory where Ember's directory is a git repository. The script builds one:

```bash
assets/scenarios/screen2-fixture.sh /tmp/screen2
cargo run -p meerora -- snapshot out.png --data-dir /tmp/screen2 --events /tmp/screen2/screen2.jsonl --view agent:ember --tab editor
cargo run -p meerora -- snapshot out.png --data-dir /tmp/screen2 --events /tmp/screen2/screen2.jsonl --view agent:ember \
    --open terminal:2,browser:2=https://example.org --tab browser:2      # more instances, like --launch it writes layouts.json
```

The Git app needs a repository with changes. This script builds a data directory where Ember has the repository as its workspace folder:

```bash
assets/scenarios/git-fixture.sh /tmp/git-shot
cargo run -p meerora -- snapshot out.png --data-dir /tmp/git-shot/data --events /tmp/git-shot/git.jsonl --view agent:ember \
    --layout fullscreen --open git:1=/tmp/git-shot/project --tab git:1                 # add --git unified for the unified diff
cargo run -p meerora -- snapshot out.png --data-dir /tmp/git-shot/data --events /tmp/git-shot/git.jsonl --view agent:ember \
    --layout fullscreen --open git:1=/tmp/git-shot/project --tab git:1 --git staged --git-message 'Retry when the archive is busy\nThe archive answers 503 under load.'
cargo run -p meerora -- snapshot out.png --data-dir /tmp/git-shot/data --view edit:ember --builder-step 2   # the workspace list
```

`figure-check` builds a single script and prints its triangles, anchors and errors. For a device it prints its place, bindings, actions and the capabilities it needs:

```bash
cargo run -p meerora-script --bin figure-check -- assets/agents/wren/figure.js
cargo run -p meerora-script --bin figure-check -- assets/spaces/workshop/devices/airplane.js
```

## Layout

| Path | Contents |
| --- | --- |
| `crates/meerora` | The app: window, world simulation, egui UI, snapshot mode |
| `crates/meerora-scene` | wgpu renderer for the space, without windowing (D15) |
| `crates/meerora-script` | Sandboxed JavaScript construction scripts on QuickJS (D4, D20). API in `API.md` |
| `crates/meerora-geom` | Low-poly primitives and meshes |
| `crates/meerora-protocol` | Daemon messages and the `agent.toml` format (D21) |
| `crates/meerora-agentd` | The agent daemon: runs, chat, schedules (D23), tools, builder, git per agent, the graveyard, mocks |
| `crates/meerora-runtime` | The claude CLI, Codex CLI and Ollama behind one interface (D22, D29) |
| `crates/meerora-mcp` | MCP bridge that gives every runtime the meerora tools (D24) |
| `crates/meerora-index` | The index server for external agents, accounts, profiles, follows and posts, and a demo A2A agent (D25 to D27, Q9) |
| `crates/meerora-term` | PTY and terminal emulation for the terminal tile |
| `crates/meerora-browser` | Headless Chrome over CDP for the browser tile (D13) |
| `crates/meerora-voice` | Local voice: recording with pw-record, whisper.cpp, Piper and the model downloads (D16) |
| `crates/meerora-compositor` | The nested Wayland compositor with Xwayland for real apps in agent views. Each agent desktop has a Wayland socket, an X display and a session bus for your apps and another set for the agent's sandboxed apps (D15 phase 2, D16) |
| `assets/agents/<id>/` | One directory per agent with `agent.toml` and `figure.js` (D21) |
| `assets/spaces/<room>/space.js` | The rooms of the space, `workshop`, `space-station` and `office`, with their stations, walk graph and graveyard plot (D6, D9) |
| `assets/spaces/<room>/devices/` | The devices of each room: plane, clock, system graph, memory plant, mailbox and noticeboard (D7, D8) |
| `assets/scenarios/` | The daemon's demo scenario and the screen 1 state for snapshots |
| `assets/templates/news/` | The news agent template: figure, instructions and default sources (D28) |
| `assets/templates/helper/` | Rivet, the helper: its `agent.toml` with instructions, the robot figure and the figure of agents it creates without one |
| `assets/commands.toml`, `assets/commands/` | The example keywords and scripts of the command box, written into a data directory the first time the box opens there |
| `scripts/` | The login session: `meerora-session`, `install-session.sh`, `uninstall-session.sh` and their test (D14, D15) |

The app reads its data from `$MEERORA_DATA_DIR`, else from `~/.local/share/meerora` when that exists, else from the shipped assets. The binaries look for those in `$MEERORA_ASSETS`, then in `share/meerora` of their own install prefix (`/usr/local/share/meerora` for `/usr/local/bin/meerora`), then in `assets/` of this repository. `meerora-agentd init` copies its data directory from the same place.

## License

AGPL-3.0-or-later. The fonts in `assets/fonts` are under the SIL Open Font License.
