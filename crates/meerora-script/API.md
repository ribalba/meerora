# meerora script API (milestone 1)

Figure, space and device scripts are JavaScript (D20). A script is an ES module that runs once on QuickJS and ends with `export default figure({...})`, `export default space({...})` or `export default device({...})`. It has no I/O, no imports, no `Date` and a seeded `Math.random`, so the same file always builds the same geometry. Limits are 32 MiB of memory and 500 ms.

Units are centimetres and y points up. Figures stand on the origin and face +z. Colours are `"#RRGGBB"` strings.

## Shapes

Every shape returns a new part. Solids sit on y = 0 and are centred on the y axis. Sizes and radii must be greater than 0. Segment counts below the minimum (3 for most shapes, 4 for `capsule` and `egg`) are raised to it, and `sphere` detail stops at 4. A run aborts once its shapes add up to more than 200,000 triangles, whatever the declared budget.

| Call | Notes |
| --- | --- |
| `box(w, h, d)` | `.bevel(r)` chamfers its edges |
| `cylinder(r, h, segments = 12)` | |
| `cone(rBottom, h, segments = 12, rTop = 0)` | `rTop > 0` makes a frustum |
| `sphere(r, detail = 1)` | icosphere, 80 triangles at detail 1 |
| `capsule(r, h, segments = 10)` | `h` is the total height |
| `egg(w, h, segments = 12)` | |
| `lathe(profile, segments = 12)` | `profile` is `[[radius, y], ...]` from bottom to top, at most 1024 points. The lowest point is moved to y = 0 and ends with a radius above 0 get a flat cap |
| `torus(R, r, segments = 16, sides = 6)` | lies flat, centred on the origin |
| `tube(points, r, sides = 6)` | `points` is `[[x, y, z], ...]`, used as given, at most 1024 points |
| `extrude(outline, depth)` | `outline` is `[[x, y], ...]` counter-clockwise in the xy plane, extruded along z around z = 0, at most 256 points. A clockwise outline is turned around |
| `quad(w, d)` | one-sided floor tile facing up |

## Part methods

Each method returns the part, so calls chain. Placement uses the current world bounds at the time of the call.

| Method | Effect |
| --- | --- |
| `.material(finish, color)` | finish is one of matte, enamel, ceramic, felt, glaze, paper, brass, metal, wood, leather, glass, plastic, stone, foliage |
| `.color(color)` | keeps the finish |
| `.glow(intensity = 1)` | emissive, for eyes, bulbs and screens. Resting figures dim glowing parts. Values above 1 count as 1 |
| `.at(x, y, z)` or `.at(point)` | moves the part so the bottom centre of its bounds is at the point |
| `.move(dx, dy, dz)` | |
| `.rotate(xDeg, yDeg, zDeg)` | around the bottom centre, in the order x, y, z |
| `.scale(s)` or `.scale(sx, sy, sz)` | around the bottom centre |
| `.on(other, {x = 0, z = 0} = {})` | bottom centre onto the top centre of `other`, shifted by x and z. `other` may also be a point |
| `.above(other, gap = 0)` | like `on`, `gap` higher |
| `.below(other, gap = 0)` | top centre to the bottom centre of `other`, `gap` lower |
| `.beside(other, side, gap = 0)` | touches the `"left"` (-x), `"right"` (+x), `"front"` (+z) or `"back"` (-z) face of `other`, bottoms level and centred along the remaining axis |
| `.add(...children)` | children follow every later move, rotation and scale of this part. A part has one parent, adding it elsewhere moves it |
| `.clone()` | independent copy including children |

`.material`, `.color` and `.glow` paint the part and the children it has at that moment. A scale factor of 0 is an error, a negative one mirrors the part.

Read-only properties:

| Property | Value |
| --- | --- |
| `part.top`, `part.bottom`, `part.center` | points at the centre of the top face, bottom face and box |
| `part.front`, `part.back`, `part.left`, `part.right` | points at the centre of the +z, -z, -x and +x faces |
| `part.bounds` | `{min: [x, y, z], max: [x, y, z]}` |

A point is an `[x, y, z]` array with `.above(d)` and `.offset(dx, dy, dz)`, both returning a new point. A plain array of three numbers is accepted wherever a point is expected.

`group(...parts)` returns a part without geometry of its own whose children are the given parts. The bounds of an empty group are the single point it was moved to.

## Library parts

Built in JavaScript on top of the calls above.

| Call | Result |
| --- | --- |
| `bentRod(length, {bend = 30, r = 1.2, color})` | thin rod that starts upright and bends `bend` degrees towards +x halfway. `.tip` is the point at its end and follows later moves |
| `bulb(color, r = 4)` | glowing sphere |
| `screen(w, h, color = "#1B2A2A")` | glowing panel 1 cm thick, for faces |
| `eyes(gap, r = 3, color = "#FFE7B0")` | two glowing discs facing +z, `gap` cm apart centre to centre |
| `treads({wheels = 3, width = 50})` | tank treads, about a third of `width` high |
| `wrench(size = 12)`, `vents(count)`, `dial()` | small details facing +z, for the robot |
| `emblem(name, {size = 10, color})` | flat sign facing +z. `name` is one of lock, wifi, bolt, star, heart, mail |

## Animations (D5, D9)

| Call | Motion |
| --- | --- |
| `bob(amplitude = 2, period = 1.4)` | up and down |
| `hop(count = 1, height = 12, period = 2.4)` | hops, then a pause |
| `breathe(period = 4, depth = 0.02)` | slow vertical squash |
| `spin(period = 4)` | turns around y |
| `wave(period = 1.5, angle = 8)` | rocks around the pivot |
| `orbit(radius = 20, period = 4)` | circles around the start point |
| `still()` | no motion |

Modifiers return the animation: `.then(face("viewer"))` turns the figure towards the camera after the motion, `.eyes("closed")` dims glowing parts.

## Params (D24)

| Call | Value |
| --- | --- |
| `toggle(default)` | true or false |
| `choice(values, default)` | one of the strings |
| `number(min, max, default)` | |
| `count(default)` | non-negative integer |
| `text(default)` | short string |

`.shows(part, when = true, {pivot})` removes `part` and its children from the always visible geometry and draws them only while the param equals `when`. `part` does not need to be listed in `parts`. `.animate(motion)` animates the part of the last `.shows` around `pivot` (default its bottom centre when `figure()` is called). `when` must be a valid value of the param. Agents change params at runtime (`figure.set`), which never re-runs the script.

## figure({...})

```js
export default figure({
  parts: [body, head],                       // children of listed parts are included
  anchors: { ring: head.top.above(14), face: visor },   // face is a part or a point
  animations: { working: bob(), waiting: hop(2).then(face("viewer")), resting: breathe() },
  params: { letter: toggle(false).shows(envelope) },    // optional
  budget: { triangles: 3000 },               // default 3000, at most 6000
});
```

The script fails if the geometry exceeds the budget, or if any of `anchors.ring`, `anchors.face`, `animations.working`, `animations.waiting` or `animations.resting` is missing (D5). All missing slots are reported together. The budget counts the always visible geometry and every param part, because all of them can show at once. A part given as the ring anchor stands for its top centre, a part given as the face anchor for the centre of its bounds. Unknown keys in `figure({...})`, `anchors` and `animations` are errors, so typos do not turn into missing slots.

## space({...})

```js
export default space({
  parts: [floor, shelf],
  walls: [{ inside: [1, 0, 0], parts: [leftWall, window] }],   // cut away while the camera is outside
  lights: [light([120, 220, 160], "#FFD9A0", 1.2, 320)],   // position, colour, intensity, radius
  ambient: { sky: "#2A3550", ground: "#1A1410" },
  sun: { direction: [-0.4, -1, -0.3], color: "#FFF1DC", intensity: 0.6 },
  background: "#0B0D12",
  stations: { desk: station([300, 0, 380], { facing: 180, front: false, tools: ["terminal", "editor"] }) },
  waypoints: { hall: [260, 0, 300] },
  paths: [["desk", "hall"]],                 // undirected, between station and waypoint ids
  camera: { target: [400, 60, 350], viewHeight: 900, azimuth: 45, elevation: 30 },
  graveyard: { station: "graveyard", gap: 70, width: 96, depth: 200 },   // optional
  budget: { triangles: 60000 },              // default 60000
});
```

`walls` lists the walls the camera may look through. The user can turn the camera around the space, and a wall between the camera and the room would hide it. Each entry has the parts of one wall together with what hangs on it (windows, doors, pictures), and `inside`, a direction along the floor from the wall into the room. The app leaves a wall out while the camera looks at it from outside. Walls count towards the triangle budget, and agents walk around them like any other part. A part listed in a wall is not drawn with `parts` as well.

Every station becomes a node of the walk graph next to the waypoints. The script fails when a path names an unknown id, when a station and a waypoint share an id or when a station cannot be reached from the others. A station may also be given as a plain point (facing 0, not a front spot, no tools).

`graveyard` declares the plot where the app stands a small tombstone for each retired agent. The plot starts `gap` cm in front of `station` in the direction the station faces, is `width` cm wide across that direction and `depth` cm deep along it. `station` defaults to `"graveyard"` and `gap` to 0. The script builds its fence around the same rectangle, higher than 25 cm and closed, so agents walk around it. The script fails when `station` is not a station. Without `graveyard` the space has no plot and the tombstones stay out, while the graveyard view still lists every retired agent. Retired agents walk to the station before they leave the space.

`tools` lists the action categories whose work happens at a station (D9). A category is the part of an action name before the dot, so `terminal.run` belongs to `terminal` and `mail.read` to `mail`. Runtimes emit `terminal`, `editor`, `files`, `browser` and `meerora` actions, and other agents may add their own categories such as `mail`. A working agent walks to the station that lists the category of its latest action. Entries are lowercase names of at most 32 letters, digits or underscores, starting with a letter. A station must not list a category twice, and two stations must not share one. The default is an empty list. `light(position, color = "#FFE2B0", intensity = 1, radius = 300)`. Left out, `ambient`, `sun` and `background` take the values in the example above, and the camera looks at the centre of the bounds from azimuth 45 and elevation 30 with a view height of 1.2 times the largest size.

## device({...}) (D7, D8)

A device is an object in the space with a shape, a live state binding and actions, for example the paper plane that follows airplane mode. Its shape is built like a figure, from parts and params with parts shown per value and their animations. Devices of the space live in `devices/<name>.js` of the room (`spaces/<room>/devices/` in the data dir, `space/devices/` for the room `custom`), devices an agent made in `agents/<id>/devices/<name>.js`. The file name is the device's id, in lowercase letters, digits, `-` and `_`.

```js
export default device({
  name: "Airplane mode",                                   // 1 to 40 characters
  parts: [sill],                                           // always visible, optional
  params: { flying: toggle(false).shows(parked, false).shows(plane, true, { pivot: [150, 92, 64] }).animate(spin(7)) },
  place: { at: [12, 104, 222], wall: [1, 0, 0] },          // or { station: "window", offset: [x, y, z] }
  bind: { flying: "network.airplane_mode" },               // param -> source
  actions: { toggle: action("network.airplane_mode.write", "toggle", "Toggle airplane mode") },
  status: { flying: { true: "On. The plane circles the room.", false: "Off. Parked on the sill." } },
  display: graph({ value: "level", at: [0, 40, 8], width: 120, height: 50 }),   // optional, see below
  budget: { triangles: 600 },                              // default 1500, at most 3000
});
```

`place` says where the device stands. Its parts are modelled around the origin and facing +z like a figure, and the app moves them there.

| Key | Meaning |
| --- | --- |
| `at: [x, y, z]` | a position in the space |
| `station: "id"` | next to a station of the space, moved by `offset` (axes of the space, default `[0, 0, 0]`) |
| `facing` | degrees around y, 0 = +z. A device at a station takes the station's facing by default, otherwise 0 |
| `wall: [x, 0, z]` | the device hangs on the wall whose `inside` is this direction and is cut away together with it (see `walls` of `space()`) |

`bind` maps a param to a state source, given as a string or as `from(source)` with steps that turn the reading into the param's value. The agent daemon reads the sources and sets the params, so a binding works both ways: switching airplane mode anywhere moves the plane.

| Source | Reading | Agent devices need |
| --- | --- | --- |
| `system.disk` | share of the home filesystem in use, 0 to 1, read every minute | `system.disk.read` |
| `system.cpu` | share of CPU time in use since the last reading, 0 to 1, from `/proc/stat`, read every few seconds. The first reading is the share since boot | `system.cpu.read` |
| `system.memory` | share of memory that is available, 0 to 1, `MemAvailable` over `MemTotal` of `/proc/meminfo`, read every few seconds | `system.memory.read` |
| `system.network` | bytes per second received and sent by every interface but loopback, from `/proc/net/dev`, read every few seconds. It spans orders of magnitude, so bind it with `.log(max)` for a level and `.bytes()` for a text | `system.network.read` |
| `network.airplane_mode` | true while every radio is off, read every few seconds | `network.airplane_mode.read` |
| `clock.time` | local time in minutes since midnight | nothing |
| `agent.<id>.param.<name>` | a figure param of an agent (D24), for example `agent.wren.param.letter` | nothing |
| `agent.<id>.ring` | `"working"`, `"needs_you"` or `"resting"` (D3) | nothing |

| Step | Result |
| --- | --- |
| `.div(n)` | whole times `n` fits into the number |
| `.mod(n)` | the rest after dividing by `n` |
| `.steps(bounds, values)` | `values[0]` below `bounds[0]`, `values[1]` up to `bounds[1]`, and so on. Bounds grow, there is one value more than bounds |
| `.is(value)` | true when the reading equals `value` |
| `.percent()` | a share as a text like `"42%"` |
| `.time()` | minutes since midnight as a text like `"09:30"` |
| `.log(max)` | a number from 0 up onto 0 to 1 on a log scale, `ln(1 + x) / ln(1 + max)`, 1 from `max` on. `from("system.network").log(100e6)` puts 10 kB/s half way to 100 MB/s |
| `.bytes()` | a number of bytes as a text like `"1.2 MB"`, in steps of 1000 |

For example the hour hand of a clock with 12 positions is `from("clock.time").div(60).mod(12)` bound to a `count()` param. The script fails when a source is unknown or when what a binding gives does not fit its param, for example a number for a `toggle()` or a step value that is not one of the choices.

A param marked with `.setting(label, labels)` is a setting: the user picks its value on the device card, and no source drives it. It works on `choice()` and `toggle()` params. `label` is the card's heading for it, and `labels` optionally names values on the card, like `{ cpu: "CPU" }`, otherwise the value is shown with a capital first letter. The daemon keeps the choice in `devices/settings.toml` of the room, beside `grants.toml`, and sends it like a bound value, so `shows()` and `status` can use it. `per(setting, cases)` binds a param to one source per value of a setting. A value without a case leaves the param alone. An agent's device needs the read capability of every source its cases name.

```js
params: {
  metric: choice(["cpu", "memory"], "cpu").setting("Shows", { cpu: "CPU", memory: "Memory free" }),
  level: number(0, 1, 0),
},
bind: { level: per("metric", { cpu: "system.cpu", memory: from("system.memory") }) },
status: { metric: { cpu: "CPU in use.", memory: "Memory free." } },
```

`display: graph({...})` makes the device a live graph. The app keeps the recent values of a `number()` or `count()` param as they arrive from the daemon and draws them as a glowing line inside a rectangle of the device. The line is built again whenever a value arrives, and it starts over when a setting of the device changes, because the old values then mean something else.

| Option | Meaning |
| --- | --- |
| `value` | the param whose values the line follows |
| `at: [x, y, z]` | the centre of the rectangle in the device's own axes. The rectangle faces +z, like a screen |
| `width`, `height` | the size of the rectangle in cm |
| `points = 60` | how many values the line keeps, 2 to 240. The newest sits on the right edge |
| `min = 0`, `max = 1` | the values at the bottom and the top edge. Values outside are clamped |
| `color = "#F2B45A"` | the colour of the line, which glows |
| `thickness = 1.6` | the thickness of the line in cm |

`actions` are the buttons of the device card, at most four. An action never runs code of the script.

| Call | Effect |
| --- | --- |
| `action(capability, does = "toggle", label)` | changes the state behind a declared capability (D8). `does` is `"toggle"`, `"on"` or `"off"`. The only capability with a backend so far is `network.airplane_mode.write` |
| `opens(view, label)` | opens a view of the app, so far only `"feed"`. Needs no capability |

The user approves the capability of a space device once, on the approval card, and the grant is kept in `devices/grants.toml` of the room. A device an agent made may only bind and use capabilities its agent holds. It does not load otherwise, and the agent reads why in its chat. Granted capabilities show as a padlock on the object.

`status` is the line of the device card that says what it shows. It is a text with `{param}` placeholders, like `"{used} of the disk is used"`, or one text per value of one param, like `{ flying: { true: "On", false: "Off" } }`.

The budget counts the parts and every param part, because all of them can show at once.

## Writing scripts

- Do not name a variable after an API function. `const face = screen(30, 18)` hides `face("viewer")`, and `const eyes = eyes(12)` fails.
- Errors name the file and line, for example `wren.js:12: TypeError: box(h) must be greater than 0, got -2`.
- `console.log` works. `figure-check` prints the output, the app drops it.
- Check a script with `cargo run -p meerora-script --bin figure-check -- path/to/figure.js`. It prints devices too, with their place, bindings, actions and the capabilities they need.

## command({...}) (the command box)

Command scripts extend the command box of the app (Alt+Space). They live in `commands/*.js` of the data directory and end with `export default command({...})`. They use the same sandbox as figures (D20): QuickJS, no imports, no file, network or process access. Unlike figures they also get `Date`, and `Math.random` is not the fixed sequence of figure scripts, because nothing caches what a command returns. None of the modelling API exists here.

A command script stays loaded. Each script has its own QuickJS context on a thread of its own, and `run` is called again for every keystroke, so a script may keep state between calls. Loading may take 500 ms, each call of `run` 200 ms, and a script has 16 MiB of memory. A call that runs over its time or memory is stopped, the row shows the error, and the script is loaded afresh before its next call. The box loads a script again when its file changes and shows load errors as a row.

```js
export default command({
  keyword: "temp",                 // typed before the query, one word
  title: "Convert temperatures",   // shown when the keyword is suggested
  subtitle: "temp 21",             // optional
  run(query) {                     // the text after "temp ", trimmed
    const c = Number(query);
    if (!Number.isFinite(c)) return [];
    const f = c * 9 / 5 + 32;
    return [{ title: `${c} °C = ${f} °F`, glyph: "°", action: copy(f), alt: copy(`${f} °F`) }];
  },
});
```

| Field | Notes |
| --- | --- |
| `keyword` | one word of letters, digits, `_ . + -`, at most 24 characters. Typing it and a space sends the rest to `run`. Tab or Enter on the suggested command writes the keyword |
| `title`, `subtitle` | text of the suggestion row |
| `run(query)` | returns a list of items, a single item, or a promise of either. At most 50 items count |

An item is `{ title, subtitle, glyph, action, alt }`. Only `title` is required, except for a `calc` item, which shows its result when the title is empty. `glyph` is one or two characters for the icon square. `action` is what Enter does and `alt` what Shift+Enter does. An item without an action is shown as information. Numbers in titles and actions become text.

Actions are plain data. meerora checks them, shows what Enter does after the item's own subtitle, so an item cannot hide what it runs (D8), and carries them out only when the user picks the item. The helpers build them:

| Helper | Data | Enter |
| --- | --- | --- |
| `openUrl(url)` | `{ type: "open_url", url }` | opens an http or https URL in the browser of the target agent |
| `launch(program, ...args)` | `{ type: "launch", program, args }` | starts the program as a real app on the target agent's desktop, as you, without a shell |
| `say(agent, text)` | `{ type: "say", agent, text }` | sends the text to the agent (id or name), like `@agent text` |
| `openApp(app)` | `{ type: "open_app", app }` | opens a built-in app such as `editor` or `terminal:2` in the target agent's view |
| `copy(text)` | `{ type: "copy", text }` | puts the text on the clipboard |
| `notify(text)` | `{ type: "notify", text }` | shows the text as a note |
| `calc(expression)` | `{ type: "calc", expression }` | evaluates the expression with the box's calculator and copies the result |

The target agent is the one whose view is open, else the one viewed last. Ctrl+Tab in the box picks another. `console.log` is accepted and dropped. Errors name the file and line like figure errors, for example `units.js:12: TypeError: query.nope is undefined`.

The two examples, `assets/commands/units.js` (temperatures) and `assets/commands/github.js` (GitHub repository, issue and search links), are copied into `commands/` the first time the box opens in a data directory without that folder.
