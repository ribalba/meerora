// meerora command scripts for the command box (Alt+Space). Evaluated
// before every command script. A script ends with
// `export default command({ keyword, title, run(query) })`, and `run`
// returns result items whose actions are plain data. meerora carries the
// actions out when the user presses Enter, a script never does (D8).
"use strict";
(() => {
  const N = globalThis.__meerora_native;
  delete globalThis.__meerora_native;
  const G = globalThis;

  const show = (v) => {
    if (typeof v === "string") return JSON.stringify(v);
    if (v === null) return "null";
    if (Array.isArray(v)) return "an array";
    if (typeof v === "function") return "a function";
    return typeof v === "object" ? "an object" : String(v);
  };
  const fail = (msg) => {
    throw new TypeError(msg);
  };
  const str = (v, what) => (typeof v === "string" ? v : fail(`${what} must be a string, got ${show(v)}`));
  const text = (v, what) => {
    if (typeof v === "string") return v;
    if (typeof v === "number" || typeof v === "boolean") return String(v);
    return fail(`${what} must be a string, got ${show(v)}`);
  };

  // The commands of this script. Only the first one counts, a script is one command.
  const commands = [];

  G.command = function command(spec) {
    if (spec === null || typeof spec !== "object" || Array.isArray(spec)) {
      fail(`command() takes an object like { keyword: "temp", title: "Temperatures", run(query) { ... } }, got ${show(spec)}`);
    }
    for (const k of Object.keys(spec)) {
      if (!["keyword", "title", "subtitle", "run"].includes(k)) {
        fail(`command() has no option ${JSON.stringify(k)}. Known options: keyword, title, subtitle, run`);
      }
    }
    const keyword = str(spec.keyword, "command() keyword");
    if (!/^[A-Za-z0-9][A-Za-z0-9_.+-]{0,23}$/.test(keyword)) {
      fail(`command() keyword must be one word of letters, digits, _ . + or -, at most 24 characters, got ${show(keyword)}`);
    }
    const title = str(spec.title, "command() title");
    if (title.trim() === "") fail("command() title must not be empty");
    const subtitle = spec.subtitle === undefined ? "" : str(spec.subtitle, "command() subtitle");
    if (typeof spec.run !== "function") fail(`command() run must be a function run(query), got ${show(spec.run)}`);
    const index = N.command(JSON.stringify({ keyword, title, subtitle }));
    commands[index] = spec;
    return Object.freeze({ __meerora_command: index });
  };

  // Actions. Each returns plain data that meerora checks and runs on Enter.
  G.openUrl = (url) => ({ type: "open_url", url: text(url, "openUrl(url)") });
  G.launch = (program, ...args) => ({
    type: "launch",
    program: text(program, "launch(program)"),
    args: args.map((a, i) => text(a, `launch() argument ${i + 1}`)),
  });
  G.say = (agent, message) => ({ type: "say", agent: text(agent, "say(agent)"), text: text(message, "say(agent, text)") });
  G.openApp = (app) => ({ type: "open_app", app: text(app, "openApp(app)") });
  G.copy = (value) => ({ type: "copy", text: text(value, "copy(text)") });
  G.notify = (message) => ({ type: "notify", text: text(message, "notify(text)") });
  G.calc = (expression) => ({ type: "calc", expression: text(expression, "calc(expression)") });

  const log = () => {};
  G.console = Object.freeze({ log, info: log, warn: log, error: log, debug: log });

  // Called by the host for every query. The result goes back as JSON. A
  // promise is waited for, within the same time limit.
  const encode = (out) => {
    if (out === undefined || out === null) return "[]";
    const list = Array.isArray(out) ? out : [out];
    return JSON.stringify(list.slice(0, 64));
  };
  G.__meerora_run = (index, query) => {
    const spec = commands[index];
    if (spec === undefined) fail("this script defines no command");
    const out = spec.run(query);
    if (out !== null && typeof out === "object" && typeof out.then === "function") return out.then(encode);
    return encode(out);
  };
})();
