// The workshop and garden, a cutaway workshop opening onto a walled garden,
// modelled on screen 1 and grown by half for room between the stations.
// Units are centimetres. The far walls stand on the x = 0 and z = 0 planes,
// the camera looks in from the +x +z corner.
//
// Station facing is in degrees around y with 0 = +z. 45 faces the camera.
//
// Everything here has a job: the floor and the walls, the furniture of the
// stations, the lights, the hedges that bound the garden and the graveyard
// plot. The live parts are devices in devices/ (D7): the paper plane on the
// window sill, the hands of the clock, the system graph on the back wall,
// the leaves of the tree, the mailbox flag and the noticeboard.

const WALL = "#E9DCC6";
const WALL_DARK = "#D8C8AE";
const FLOOR = "#9A6A44";
const FLOOR_ALT = "#8C5F3C";
const WOOD = "#6E4A2E";
const WOOD_LIGHT = "#A87A50";
const SLAB = "#1A1E27";
const GRASS = "#4F7A45";
const HEDGE = "#2F5634";
const STONE = "#B9B2A4";
const METAL = "#2A2F3B";
const INK = "#14171E";

const HOUSE_X = 840; // the house floor spans x 0..840, the garden 840..1350
const WIDTH = 1350;
const DEPTH = 1080;
const WALL_H = 300;
const WALL_T = 14;

const parts = [];
const add = (...p) => {
  parts.push(...p);
  return p[0];
};
// Each wall with everything that hangs on it. The app cuts a wall away
// while the camera looks at it from outside the house.
const leftWall = [];
const backWall = [];
const onWall = (wall, ...p) => {
  wall.push(...p);
  return p[0];
};

// Base slab, floor boards and grass.
add(box(WIDTH, 36, DEPTH).material("stone", SLAB).at(WIDTH / 2, -38, DEPTH / 2));
const boards = 12;
for (let i = 0; i < boards; i++) {
  const w = HOUSE_X / boards;
  add(box(w - 0.6, 2, DEPTH).material("wood", i % 2 ? FLOOR : FLOOR_ALT).at(w * i + w / 2, -2, DEPTH / 2));
}
add(box(WIDTH - HOUSE_X, 3, DEPTH).material("foliage", GRASS).at(HOUSE_X + (WIDTH - HOUSE_X) / 2, -3, DEPTH / 2));

// Walls: left (x = 0) and back (z = 0).
onWall(leftWall, box(WALL_T, WALL_H, DEPTH).material("matte", WALL).at(-WALL_T / 2, 0, DEPTH / 2));
onWall(backWall, box(HOUSE_X, WALL_H, WALL_T).material("matte", WALL_DARK).at(HOUSE_X / 2, 0, -WALL_T / 2));

// Window in the left wall, night outside. The plane device parks on its sill.
const WINDOW_Z = 330;
onWall(leftWall, box(6, 140, 170).material("wood", WOOD).at(3, 100, WINDOW_Z));
onWall(leftWall, box(3, 126, 156).material("glass", "#1B2A4A").glow(0.35).at(5, 107, WINDOW_Z));
onWall(leftWall, box(24, 5, 184).material("wood", WOOD_LIGHT).at(12, 99, WINDOW_Z));

// Door to the outside world, where external agents arrive.
const DOOR_Z = 690;
onWall(leftWall, box(8, 236, 112).material("wood", WOOD).at(4, 0, DOOR_Z));
onWall(leftWall, box(6, 226, 98).material("enamel", "#1F6F6B").at(8, 0, DOOR_Z));
onWall(leftWall, sphere(3.4, 1).material("brass", "#F2B45A").at(14, 108, DOOR_Z + 36));

// Clock face on the back wall. Its hands are the device devices/clock.js.
onWall(backWall, cylinder(26, 5, 18).material("ceramic", "#F4F1EA").rotate(90, 0, 0).at(470, 214, 3));

// Bookshelf against the back wall, Claude's station: the frame and its shelves.
add(box(160, 210, 38).material("wood", WOOD).at(250, 0, 21));
for (let level = 0; level < 4; level++) {
  add(box(150, 3, 34).material("wood", WOOD_LIGHT).at(250, 12 + level * 50, 25));
}

// Router cabinet in the back corner, where Rivet fixes the wifi. Its lights
// blink in the colours of the screen above.
add(box(70, 132, 50).material("metal", METAL).bevel(2).at(770, 0, 32));
for (let i = 0; i < 4; i++) {
  add(box(3, 3, 2).material("glass", i % 2 ? "#7FE3D0" : "#F2B45A").glow(1).at(748 + i * 10, 116, 58));
}
add(cylinder(1.2, 36, 6).material("metal", INK).at(752, 132, 38));
add(cylinder(1.2, 36, 6).material("metal", INK).at(788, 132, 38));

// Workbench with a vise, Ember's station.
const BENCH = [585, 465];
add(box(200, 9, 84).material("wood", WOOD_LIGHT).at(BENCH[0], 80, BENCH[1]));
for (const [dx, dz] of [[-92, -34], [92, -34], [-92, 34], [92, 34]]) {
  add(box(8, 80, 8).material("wood", WOOD).at(BENCH[0] + dx, 0, BENCH[1] + dz));
}
add(box(18, 12, 22).material("metal", "#4466A0").at(BENCH[0] + 80, 89, BENCH[1] + 30));

// Desk with the focus lamp, Quill's station.
const DESK = [330, 640];
add(box(150, 6, 76).material("wood", WOOD).at(DESK[0], 72, DESK[1]));
for (const [dx, dz] of [[-68, -32], [68, -32], [-68, 32], [68, 32]]) {
  add(box(6, 72, 6).material("wood", WOOD).at(DESK[0] + dx, 0, DESK[1] + dz));
}
const lamp = [DESK[0] + 52, DESK[1] - 22];
add(cylinder(8, 2, 12).material("metal", INK).at(lamp[0], 78, lamp[1]));
add(tube([[lamp[0], 80, lamp[1]], [lamp[0], 118, lamp[1]], [lamp[0] - 16, 132, lamp[1] + 8]], 1.4, 5).material("metal", INK));
add(cone(14, 14, 12, 4).material("metal", INK).rotate(180, 0, 0).at(lamp[0] - 18, 136, lamp[1] + 9));
add(sphere(4.5, 1).material("glass", "#FFE2B0").glow(1).at(lamp[0] - 18, 120, lamp[1] + 9));

// The front step, where waiting agents gather (D3).
const STEP = [500, 1030];
add(box(220, 10, 80).material("stone", STONE).at(STEP[0], 0, STEP[1]));

// Garden: hedges along the far edges, a tree, a bench, the mailbox and a lantern.
add(box(WIDTH - HOUSE_X, 90, 36).material("foliage", HEDGE).bevel(6).at(HOUSE_X + (WIDTH - HOUSE_X) / 2, 0, 18));
add(box(32, 36, DEPTH - 36).material("foliage", HEDGE).bevel(6).at(WIDTH - 16, 0, 36 + (DEPTH - 36) / 2));
// The tree stands in a raised bed, so agents walk around it. Its leaves are
// the device devices/tree.js and follow the free memory.
const TREE = [1060, 300];
add(cylinder(60, 28, 14).material("stone", "#8E877A").at(TREE[0], 0, TREE[1]));
add(cylinder(54, 1, 14).material("foliage", "#3B2A1E").at(TREE[0], 28, TREE[1]));
add(cylinder(10, 100, 7).material("wood", "#5A3F2A").at(TREE[0], 0, TREE[1]));
const SEAT = [1175, 700];
add(box(130, 6, 42).material("wood", WOOD_LIGHT).at(SEAT[0], 42, SEAT[1]));
add(box(130, 34, 6).material("wood", WOOD_LIGHT).at(SEAT[0], 50, SEAT[1] - 20));
for (const dx of [-58, 58]) {
  add(box(6, 42, 38).material("wood", WOOD).at(SEAT[0] + dx, 0, SEAT[1]));
}
// The mailbox. Its flag is the device devices/mailbox.js.
const MAIL = [975, 1000];
add(cylinder(4, 92, 6).material("wood", WOOD).at(MAIL[0], 0, MAIL[1]));
add(box(36, 26, 22).material("enamel", "#2F4E3C").bevel(3).at(MAIL[0], 92, MAIL[1]));
// Garden lantern beside the bench.
const LANTERN = [1290, 590];
add(cylinder(3, 74, 6).material("metal", INK).at(LANTERN[0], 0, LANTERN[1]));
add(box(14, 18, 14).material("glass", "#FFD9A0").glow(0.9).at(LANTERN[0], 74, LANTERN[1]));

// The graveyard in the back right corner, between the tree and the hedges.
// Retired agents rest here, and the app stands a small tombstone for each in
// rows on the plot declared in space() below: GRAVE_W wide and GRAVE_D deep,
// starting GRAVE_GAP in front of the graveyard station in the direction it
// faces (-z). The fence reaches above 25 cm, so agents walk around it.
const GRAVE = [1245, 0, 450];
const GRAVE_GAP = 70;
const GRAVE_W = 96;
const GRAVE_D = 200;
{
  const IRON = "#2A2D33";
  const x0 = GRAVE[0] - GRAVE_W / 2 - 4;
  const x1 = GRAVE[0] + GRAVE_W / 2 + 4;
  const z1 = GRAVE[2] - GRAVE_GAP + 4;
  const z0 = GRAVE[2] - GRAVE_GAP - GRAVE_D - 4;
  const cx = (x0 + x1) / 2;
  add(box(x1 - x0, 1.5, z1 - z0).material("foliage", "#3E5C37").at(cx, 0, (z0 + z1) / 2));
  // An axis-aligned bar from (ax, az) to (bx, bz), t thick and h high at y.
  const bar = (ax, az, bx, bz, y, h, t, finish, color) =>
    add(box(Math.abs(bx - ax) || t, h, Math.abs(bz - az) || t).material(finish, color).at((ax + bx) / 2, y, (az + bz) / 2));
  // A low stone curb, and an iron fence with two rails that leave a gap for
  // the gate in front, too narrow to walk through.
  const gate = 12;
  bar(x0 - 3, z0, x1 + 3, z0, 0, 5, 6, "stone", STONE);
  bar(x0 - 3, z1, x1 + 3, z1, 0, 5, 6, "stone", STONE);
  bar(x0, z0, x0, z1, 0, 5, 6, "stone", STONE);
  bar(x1, z0, x1, z1, 0, 5, 6, "stone", STONE);
  for (const y of [14, 29]) {
    bar(x0, z0, x1, z0, y, 2, 2, "metal", IRON);
    bar(x0, z0, x0, z1, y, 2, 2, "metal", IRON);
    bar(x1, z0, x1, z1, y, 2, 2, "metal", IRON);
    bar(x0, z1, cx - gate, z1, y, 2, 2, "metal", IRON);
    bar(cx + gate, z1, x1, z1, y, 2, 2, "metal", IRON);
  }
  const posts = (from, to, at, alongX) => {
    const n = Math.max(1, Math.round(Math.abs(to - from) / 20));
    for (let i = 0; i <= n; i++) {
      const t = from + ((to - from) * i) / n;
      add(box(2.4, 34, 2.4).material("metal", IRON).at(alongX ? t : at, 0, alongX ? at : t));
    }
  };
  posts(x0, x1, z0, true);
  posts(z0 + 20, z1 - 20, x0, false);
  posts(z0 + 20, z1 - 20, x1, false);
  posts(x0, cx - gate, z1, true);
  posts(cx + gate, x1, z1, true);
  for (const x of [cx - gate, cx + gate]) {
    add(box(5, 42, 5).material("stone", STONE).bevel(1).at(x, 0, z1));
  }
}

export default space({
  parts,
  walls: [
    { inside: [1, 0, 0], parts: leftWall },
    { inside: [0, 0, 1], parts: backWall },
  ],
  lights: [
    light([430, 250, 500], "#FFD9A6", 1.3, 820),
    light([lamp[0] - 18, 116, lamp[1] + 9], "#FFE2B0", 0.9, 240),
    light([650, 175, 70], "#F2B45A", 0.45, 240),
    light([LANTERN[0], 88, LANTERN[1]], "#FFD9A0", 0.75, 340),
    light([770, 124, 110], "#7FE3D0", 0.25, 160),
  ],
  ambient: { sky: "#2B3550", ground: "#1C1612" },
  sun: { direction: [-0.35, -1, -0.45], color: "#BFD0FF", intensity: 0.35 },
  background: "#0B0D12",
  stations: {
    door: station([85, 0, DOOR_Z], { facing: 90 }),
    window: station([105, 0, WINDOW_Z + 15], { facing: 70 }),
    bookshelf: station([255, 0, 150], { facing: 30, tools: ["browser", "files"] }),
    router_cabinet: station([735, 0, 170], { facing: 120, tools: ["network", "system"] }),
    workbench: station([755, 0, BENCH[1]], { facing: 270, tools: ["terminal"] }),
    desk: station([320, 0, DESK[1] - 90], { facing: 20, tools: ["editor"] }),
    front_step: station([STEP[0], 10, STEP[1]], { facing: 45, front: true }),
    mailbox: station([MAIL[0] + 70, 0, MAIL[1] - 25], { facing: 290, tools: ["mail"] }),
    garden_bench: station([SEAT[0], 0, SEAT[1] + 70], { facing: 30 }),
    tree: station([TREE[0] - 50, 0, TREE[1] + 140], { facing: 45 }),
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
