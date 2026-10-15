// The space station: a cutaway crew module that opens onto a hydroponics
// bay. It has the stations of every room (D6), so homes and tools work the
// same here. Units are centimetres. The hull walls stand on the x = 0 and
// z = 0 planes, the camera looks in from the +x +z corner.
//
// Station facing is in degrees around y with 0 = +z. 45 faces the camera.
//
// Everything here has a job: the deck and the hull, the furniture of the
// stations, the lights, the bulkheads that bound the bay and the memorial
// bay with the plot of the graveyard. The live parts are devices in
// devices/ (D7): the shuttle on the viewport ledge, the hands of the clock,
// the system graph, the hydroponic plant, the comms beacon and the message
// board.

const HULL = "#C9CED6";
const HULL_DARK = "#B4BAC4";
const DECK = "#3A4250";
const DECK_ALT = "#343B48";
const BAY = "#2C3A35";
const BAY_ALT = "#28352F";
const STEEL = "#8A94A6";
const DARK = "#2A2F3B";
const INK = "#14171E";
const BULKHEAD = "#59606C";
const CYAN = "#7FE3D0";
const BLUE = "#8CD3FF";

const MODULE_X = 840; // the module spans x 0..840, the bay 840..1350
const WIDTH = 1350;
const DEPTH = 1080;
const WALL_H = 300;
const WALL_T = 14;

const parts = [];
const add = (...p) => {
  parts.push(...p);
  return p[0];
};
// Each hull wall with everything that hangs on it. The app cuts a wall away
// while the camera looks at it from outside the module.
const leftWall = [];
const backWall = [];
const onWall = (wall, ...p) => {
  wall.push(...p);
  return p[0];
};

// Base, deck plates of the module and the grating of the bay.
add(box(WIDTH, 36, DEPTH).material("metal", "#161A22").at(WIDTH / 2, -38, DEPTH / 2));
const PLATE = 120;
for (let x = 0; x < MODULE_X; x += PLATE) {
  for (let z = 0; z < DEPTH; z += PLATE) {
    const w = Math.min(PLATE, MODULE_X - x);
    const d = Math.min(PLATE, DEPTH - z);
    add(box(w - 1, 2, d - 1).material("metal", ((x + z) / PLATE) % 2 ? DECK : DECK_ALT).at(x + w / 2, -2, z + d / 2));
  }
}
for (let i = 0; i < 5; i++) {
  const w = (WIDTH - MODULE_X) / 5;
  add(box(w - 1, 3, DEPTH).material("metal", i % 2 ? BAY : BAY_ALT).at(MODULE_X + w * i + w / 2, -3, DEPTH / 2));
}

// Hull walls: left (x = 0) and back (z = 0).
onWall(leftWall, box(WALL_T, WALL_H, DEPTH).material("matte", HULL).at(-WALL_T / 2, 0, DEPTH / 2));
onWall(backWall, box(MODULE_X, WALL_H, WALL_T).material("matte", HULL_DARK).at(MODULE_X / 2, 0, -WALL_T / 2));

// The viewport in the left wall, space outside. The shuttle docks on its ledge.
const VIEW_Z = 330;
onWall(leftWall, box(8, 150, 190).material("metal", DARK).bevel(4).at(4, 92, VIEW_Z));
onWall(leftWall, box(3, 130, 170).material("glass", "#0B1430").glow(0.4).at(7, 102, VIEW_Z));
onWall(leftWall, box(26, 6, 200).material("metal", STEEL).at(13, 92, VIEW_Z));

// The airlock, where external agents come aboard: frame, hatch and its lamp.
const LOCK_Z = 690;
onWall(leftWall, box(12, 244, 132).material("metal", DARK).bevel(6).at(6, 0, LOCK_Z));
onWall(leftWall, box(8, 222, 104).material("enamel", "#D9DDE4").bevel(3).at(10, 4, LOCK_Z));
onWall(leftWall, box(4, 8, 30).material("glass", CYAN).glow(1).at(14, 232, LOCK_Z));

// Clock face on the back wall. Its hands are the device devices/clock.js.
onWall(backWall, cylinder(28, 5, 18).material("metal", STEEL).rotate(90, 0, 0).at(470, 212, 2));
onWall(backWall, cylinder(24, 6, 18).material("ceramic", "#E8ECF2").rotate(90, 0, 0).at(470, 216, 3));

// Data shelves against the back wall, Claude's station: the rack, its
// shelves and the light strip of each.
add(box(160, 212, 40).material("metal", DARK).at(250, 0, 22));
for (let level = 0; level < 4; level++) {
  add(box(150, 3, 36).material("metal", STEEL).at(250, 14 + level * 50, 26));
  add(box(140, 2, 1).material("glass", BLUE).glow(0.8).at(250, 18 + level * 50, 44));
}

// The server rack in the back corner, where Rivet keeps the links up.
add(box(72, 190, 60).material("metal", "#1E232D").bevel(2).at(770, 0, 34));
for (let i = 0; i < 5; i++) {
  add(box(56, 2, 1).material("glass", i % 2 ? CYAN : "#F2B45A").glow(0.9).at(770, 40 + i * 30, 64.5));
}

// The maintenance bench with a tool arm, Ember's station.
const BENCH = [585, 465];
add(box(200, 8, 84).material("metal", STEEL).at(BENCH[0], 80, BENCH[1]));
for (const [dx, dz] of [[-90, -34], [90, -34], [-90, 34], [90, 34]]) {
  add(box(8, 80, 8).material("metal", DARK).at(BENCH[0] + dx, 0, BENCH[1] + dz));
}
add(tube([[BENCH[0] - 80, 88, BENCH[1] - 30], [BENCH[0] - 80, 130, BENCH[1] - 30], [BENCH[0] - 40, 140, BENCH[1] - 10]], 2, 6).material("metal", DARK));
add(box(16, 10, 16).material("metal", "#E0675A").at(BENCH[0] - 40, 130, BENCH[1] - 10));

// The console desk, Quill's station: a desk on a pedestal with a slanted screen.
const DESK = [330, 640];
add(box(150, 6, 76).material("metal", "#D9DDE4").at(DESK[0], 72, DESK[1]));
add(box(50, 72, 46).material("metal", DARK).at(DESK[0], 0, DESK[1] + 6));
add(box(132, 44, 4).material("glass", "#10324A").glow(0.7).rotate(-24, 0, 0).at(DESK[0], 78, DESK[1] + 26));

// The docking pad at the front, where waiting agents gather (D3).
const PAD = [500, 990];
add(cylinder(82, 8, 24).material("metal", "#4A5262").at(PAD[0], 0, PAD[1]));
add(torus(72, 1.6, 32, 4).material("glass", CYAN).glow(0.8).at(PAD[0], 8, PAD[1]));

// The bay: bulkheads along the far edges, the hydroponic planter, the crew
// bench, the comms box and the grow lamp.
add(box(WIDTH - MODULE_X, 96, 36).material("metal", BULKHEAD).bevel(4).at(MODULE_X + (WIDTH - MODULE_X) / 2, 0, 18));
add(box(32, 40, DEPTH - 36).material("metal", BULKHEAD).bevel(4).at(WIDTH - 16, 0, 36 + (DEPTH - 36) / 2));
// The plant grows in a raised planter, so agents walk around it. Its leaves
// are the device devices/tree.js and follow the free memory.
const PLANT = [1060, 300];
add(cylinder(60, 30, 16).material("metal", STEEL).at(PLANT[0], 0, PLANT[1]));
add(cylinder(55, 1, 16).material("foliage", "#3B2A1E").at(PLANT[0], 30, PLANT[1]));
add(cylinder(4, 70, 6).material("foliage", "#4E7A3A").at(PLANT[0], 30, PLANT[1]));
const GROW = [PLANT[0] + 70, PLANT[1] - 70];
add(cylinder(3, 200, 6).material("metal", DARK).at(GROW[0], 0, GROW[1]));
add(tube([[GROW[0], 200, GROW[1]], [PLANT[0] + 20, 214, PLANT[1] - 20]], 2, 6).material("metal", DARK));
add(box(40, 6, 40).material("glass", "#E9B8FF").glow(0.9).at(PLANT[0] + 16, 208, PLANT[1] - 16));
const SEAT = [1175, 700];
add(box(130, 6, 42).material("metal", STEEL).at(SEAT[0], 42, SEAT[1]));
add(box(130, 34, 6).material("metal", STEEL).at(SEAT[0], 50, SEAT[1] - 20));
for (const dx of [-58, 58]) {
  add(box(6, 42, 38).material("metal", DARK).at(SEAT[0] + dx, 0, SEAT[1]));
}
// The comms box. Its beacon is the device devices/mailbox.js.
const COMMS = [975, 1000];
add(cylinder(5, 92, 8).material("metal", DARK).at(COMMS[0], 0, COMMS[1]));
add(box(36, 26, 24).material("metal", "#3E5A7A").bevel(3).at(COMMS[0], 92, COMMS[1]));
// The bay lamp beside the bench.
const LAMP = [1290, 590];
add(cylinder(3, 76, 6).material("metal", INK).at(LAMP[0], 0, LAMP[1]));
add(box(14, 16, 14).material("glass", "#CFE6FF").glow(0.9).at(LAMP[0], 76, LAMP[1]));

// The memorial bay in the back right corner, between the planter and the
// bulkheads. Retired agents rest here, and the app stands a small tombstone
// for each in rows on the plot declared in space() below: GRAVE_W wide and
// GRAVE_D deep, starting GRAVE_GAP in front of the graveyard station in the
// direction it faces (-z). The rail reaches above 25 cm, so agents walk
// around it. A memorial light stands behind the plot.
const GRAVE = [1245, 0, 450];
const GRAVE_GAP = 70;
const GRAVE_W = 96;
const GRAVE_D = 200;
{
  const x0 = GRAVE[0] - GRAVE_W / 2 - 4;
  const x1 = GRAVE[0] + GRAVE_W / 2 + 4;
  const z1 = GRAVE[2] - GRAVE_GAP + 4;
  const z0 = GRAVE[2] - GRAVE_GAP - GRAVE_D - 4;
  const cx = (x0 + x1) / 2;
  add(box(x1 - x0, 1.5, z1 - z0).material("metal", "#1E2430").at(cx, 0, (z0 + z1) / 2));
  // An axis-aligned bar from (ax, az) to (bx, bz), t thick and h high at y.
  const bar = (ax, az, bx, bz, y, h, t, finish, color) =>
    add(box(Math.abs(bx - ax) || t, h, Math.abs(bz - az) || t).material(finish, color).at((ax + bx) / 2, y, (az + bz) / 2));
  // A low frame and a rail with a narrow gap at the front, too narrow to walk through.
  const gate = 12;
  bar(x0 - 3, z0, x1 + 3, z0, 0, 5, 6, "metal", STEEL);
  bar(x0 - 3, z1, x1 + 3, z1, 0, 5, 6, "metal", STEEL);
  bar(x0, z0, x0, z1, 0, 5, 6, "metal", STEEL);
  bar(x1, z0, x1, z1, 0, 5, 6, "metal", STEEL);
  for (const y of [16, 32]) {
    bar(x0, z0, x1, z0, y, 2, 2, "metal", DARK);
    bar(x0, z0, x0, z1, y, 2, 2, "metal", DARK);
    bar(x1, z0, x1, z1, y, 2, 2, "metal", DARK);
    bar(x0, z1, cx - gate, z1, y, 2, 2, "metal", DARK);
    bar(cx + gate, z1, x1, z1, y, 2, 2, "metal", DARK);
  }
  const posts = (from, to, at, alongX) => {
    const n = Math.max(1, Math.round(Math.abs(to - from) / 40));
    for (let i = 0; i <= n; i++) {
      const t = from + ((to - from) * i) / n;
      add(box(3, 36, 3).material("metal", DARK).at(alongX ? t : at, 0, alongX ? at : t));
    }
  };
  posts(x0, x1, z0, true);
  posts(z0 + 40, z1 - 40, x0, false);
  posts(z0 + 40, z1 - 40, x1, false);
  posts(x0, cx - gate, z1, true);
  posts(cx + gate, x1, z1, true);
  // The memorial light behind the plot.
  add(box(10, 120, 10).material("metal", DARK).at(cx, 0, z0 - 40));
  add(box(14, 14, 14).material("glass", "#BFD9FF").glow(1).at(cx, 120, z0 - 40));
}

export default space({
  parts,
  walls: [
    { inside: [1, 0, 0], parts: leftWall },
    { inside: [0, 0, 1], parts: backWall },
  ],
  lights: [
    light([430, 260, 500], "#DCE8FF", 1.4, 880),
    light([DESK[0], 120, DESK[1]], BLUE, 0.6, 240),
    light([770, 140, 120], CYAN, 0.35, 200),
    light([PLANT[0] + 16, 200, PLANT[1] - 16], "#E9B8FF", 0.6, 280),
    light([LAMP[0], 90, LAMP[1]], "#CFE6FF", 0.8, 380),
    light([GRAVE[0], 130, GRAVE[2] - GRAVE_GAP - GRAVE_D - 44], "#BFD9FF", 0.45, 240),
  ],
  ambient: { sky: "#33405E", ground: "#16181F" },
  sun: { direction: [-0.35, -1, -0.45], color: "#C8D8FF", intensity: 0.3 },
  background: "#070910",
  stations: {
    door: station([85, 0, LOCK_Z], { facing: 90 }),
    window: station([105, 0, VIEW_Z + 15], { facing: 70 }),
    bookshelf: station([255, 0, 150], { facing: 30, tools: ["browser", "files"] }),
    router_cabinet: station([735, 0, 180], { facing: 120, tools: ["network", "system"] }),
    workbench: station([755, 0, BENCH[1]], { facing: 270, tools: ["terminal"] }),
    desk: station([320, 0, DESK[1] - 90], { facing: 20, tools: ["editor"] }),
    front_step: station([PAD[0], 8, PAD[1]], { facing: 45, front: true }),
    mailbox: station([COMMS[0] + 70, 0, COMMS[1] - 25], { facing: 290, tools: ["mail"] }),
    garden_bench: station([SEAT[0], 0, SEAT[1] + 70], { facing: 30 }),
    tree: station([PLANT[0] - 50, 0, PLANT[1] + 140], { facing: 45 }),
    graveyard: station(GRAVE, { facing: 180 }),
  },
  waypoints: {
    hall: [460, 0, 450],
    back: [580, 0, 250],
    entry: [500, 0, 880],
    gate: [900, 0, 780],
    lawn: [1080, 0, 600],
  },
  paths: [
    ["door", "hall"],
    ["window", "hall"],
    ["bookshelf", "hall"],
    ["hall", "back"],
    ["back", "router_cabinet"],
    ["back", "workbench"],
    ["workbench", "hall"],
    ["hall", "desk"],
    ["hall", "entry"],
    ["desk", "entry"],
    ["entry", "front_step"],
    ["entry", "gate"],
    ["gate", "mailbox"],
    ["gate", "lawn"],
    ["lawn", "garden_bench"],
    ["lawn", "tree"],
    ["lawn", "graveyard"],
  ],
  graveyard: { station: "graveyard", gap: GRAVE_GAP, width: GRAVE_W, depth: GRAVE_D },
  camera: { target: [680, 60, 540], viewHeight: 1520, azimuth: 45, elevation: 30 },
  budget: { triangles: 60000 },
});
