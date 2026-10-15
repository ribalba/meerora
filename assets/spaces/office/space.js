// The office: a cutaway open office with a lounge, a reception at the front
// and a quiet corner. It has the stations of every room (D6), so homes and
// tools work the same here. Units are centimetres. The walls stand on the
// x = 0 and z = 0 planes, the camera looks in from the +x +z corner.
//
// Station facing is in degrees around y with 0 = +z. 45 faces the camera.
//
// Everything here has a job: the floors and the walls, the furniture of the
// stations, the lights, the partitions that bound the lounge and the quiet
// corner with the plot of the graveyard. The live parts are devices in
// devices/ (D7): the paper plane on the window sill, the hands of the
// clock, the system graph, the large plant, the letter in the mail tray and
// the noticeboard.

const WALL = "#ECE8E1";
const WALL_DARK = "#DDD8CF";
const CARPET = "#4A5466";
const CARPET_ALT = "#465060";
const OAK = "#B08A60";
const OAK_ALT = "#A88258";
const WOOD = "#7A5638";
const STEEL = "#9AA1B0";
const DARK = "#2A2F3B";
const INK = "#14171E";
const FABRIC = "#5D6A86";
const PARTITION = "#6B7385";

const OFFICE_X = 840; // the open office spans x 0..840, the lounge 840..1350
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
// while the camera looks at it from outside the office.
const leftWall = [];
const backWall = [];
const onWall = (wall, ...p) => {
  wall.push(...p);
  return p[0];
};

// Base, carpet strips of the office and the oak boards of the lounge.
add(box(WIDTH, 36, DEPTH).material("stone", "#1A1E27").at(WIDTH / 2, -38, DEPTH / 2));
for (let i = 0; i < 7; i++) {
  const w = OFFICE_X / 7;
  add(box(w - 0.6, 2, DEPTH).material("felt", i % 2 ? CARPET : CARPET_ALT).at(w * i + w / 2, -2, DEPTH / 2));
}
for (let i = 0; i < 6; i++) {
  const w = (WIDTH - OFFICE_X) / 6;
  add(box(w - 0.6, 3, DEPTH).material("wood", i % 2 ? OAK : OAK_ALT).at(OFFICE_X + w * i + w / 2, -3, DEPTH / 2));
}

// Walls: left (x = 0) and back (z = 0).
onWall(leftWall, box(WALL_T, WALL_H, DEPTH).material("matte", WALL).at(-WALL_T / 2, 0, DEPTH / 2));
onWall(backWall, box(OFFICE_X, WALL_H, WALL_T).material("matte", WALL_DARK).at(OFFICE_X / 2, 0, -WALL_T / 2));

// A tall window in the left wall, the city at night outside. The paper plane parks on its sill.
const WINDOW_Z = 330;
onWall(leftWall, box(6, 170, 180).material("metal", STEEL).at(3, 86, WINDOW_Z));
onWall(leftWall, box(3, 158, 168).material("glass", "#1B2A4A").glow(0.35).at(5, 92, WINDOW_Z));
onWall(leftWall, box(22, 5, 190).material("wood", OAK).at(11, 86, WINDOW_Z));

// The door to the corridor, where external agents come in.
const DOOR_Z = 690;
onWall(leftWall, box(8, 232, 112).material("metal", STEEL).at(4, 0, DOOR_Z));
onWall(leftWall, box(6, 224, 98).material("wood", WOOD).at(8, 0, DOOR_Z));
onWall(leftWall, box(4, 3, 16).material("metal", "#D0D4DC").at(13, 104, DOOR_Z + 34));

// Clock face on the back wall. Its hands are the device devices/clock.js.
onWall(backWall, cylinder(26, 5, 18).material("ceramic", "#FAFAF7").rotate(90, 0, 0).at(470, 214, 3));

// The bookcase against the back wall, Claude's station: closed doors below,
// open shelves above.
add(box(160, 210, 40).material("wood", OAK).at(250, 0, 22));
add(box(150, 70, 2).material("wood", OAK_ALT).at(250, 6, 43));
for (let level = 0; level < 3; level++) {
  add(box(150, 3, 36).material("wood", WOOD).at(250, 86 + level * 42, 26));
}

// The server cabinet in the back corner, where Rivet keeps the network up.
add(box(72, 186, 64).material("metal", DARK).bevel(2).at(770, 0, 34));
add(box(60, 170, 2).material("glass", "#16303A").glow(0.25).at(770, 8, 67));
for (let i = 0; i < 4; i++) {
  add(box(3, 3, 2).material("glass", i % 2 ? "#7FE3D0" : "#F2B45A").glow(1).at(752 + i * 12, 168, 69));
}

// The workbench with a vise, Ember's station.
const BENCH = [585, 465];
add(box(200, 7, 84).material("wood", OAK).at(BENCH[0], 90, BENCH[1]));
for (const [dx, dz] of [[-92, -34], [92, -34], [-92, 34], [92, 34]]) {
  add(box(6, 90, 6).material("metal", STEEL).at(BENCH[0] + dx, 0, BENCH[1] + dz));
}
add(box(18, 12, 22).material("metal", "#4466A0").at(BENCH[0] + 80, 97, BENCH[1] + 30));

// The desk with a monitor and the desk lamp, Quill's station.
const DESK = [330, 640];
add(box(150, 5, 76).material("wood", "#E2D6C4").at(DESK[0], 72, DESK[1]));
for (const dx of [-70, 70]) {
  add(box(5, 72, 70).material("metal", STEEL).at(DESK[0] + dx, 0, DESK[1]));
}
add(box(6, 22, 6).material("metal", DARK).at(DESK[0] - 10, 77, DESK[1] + 24));
add(box(70, 40, 3).material("metal", DARK).at(DESK[0] - 10, 96, DESK[1] + 26));
add(box(64, 34, 1).material("glass", "#1A3550").glow(0.6).at(DESK[0] - 10, 99, DESK[1] + 24));
const lamp = [DESK[0] + 56, DESK[1] - 10];
add(cylinder(7, 2, 12).material("metal", INK).at(lamp[0], 77, lamp[1]));
add(tube([[lamp[0], 79, lamp[1]], [lamp[0], 116, lamp[1]], [lamp[0] - 14, 128, lamp[1] - 6]], 1.3, 5).material("metal", INK));
add(cone(12, 12, 12, 4).material("metal", INK).rotate(180, 0, 0).at(lamp[0] - 16, 132, lamp[1] - 7));
add(sphere(4, 1).material("glass", "#FFE2B0").glow(1).at(lamp[0] - 16, 118, lamp[1] - 7));

// The reception at the front: a counter and the mat where waiting agents gather (D3).
const MAT = [520, 1000];
add(box(240, 2, 90).material("felt", "#8E6A4A").at(MAT[0], 0, MAT[1]));
add(box(150, 100, 52).material("wood", WOOD).at(250, 0, 960));
add(box(160, 4, 60).material("wood", "#E2D6C4").at(250, 100, 960));

// The lounge: partitions along the far edges, the large plant, the sofa,
// the mail tray and a floor lamp.
add(box(WIDTH - OFFICE_X, 120, 20).material("felt", PARTITION).at(OFFICE_X + (WIDTH - OFFICE_X) / 2, 0, 10));
add(box(20, 120, DEPTH - 20).material("felt", PARTITION).at(WIDTH - 10, 0, 20 + (DEPTH - 20) / 2));
// The plant stands in a large pot, so agents walk around it. Its leaves are
// the device devices/tree.js and follow the free memory.
const PLANT = [1060, 300];
add(cone(36, 52, 14, 44).material("ceramic", "#D8CFC2").at(PLANT[0], 0, PLANT[1]));
add(cylinder(40, 1, 14).material("foliage", "#3B2A1E").at(PLANT[0], 52, PLANT[1]));
add(cylinder(5, 60, 6).material("wood", "#5A3F2A").at(PLANT[0], 52, PLANT[1]));
// The sofa.
const SOFA = [1175, 700];
add(box(160, 40, 70).material("felt", FABRIC).at(SOFA[0], 0, SOFA[1]));
add(box(160, 44, 18).material("felt", FABRIC).at(SOFA[0], 40, SOFA[1] - 26));
for (const dx of [-74, 74]) {
  add(box(14, 26, 70).material("felt", "#536079").at(SOFA[0] + dx, 40, SOFA[1]));
}
// The mail tray on its cabinet. The letter in it is the device devices/mailbox.js.
const MAIL = [975, 1000];
add(box(52, 90, 40).material("wood", OAK_ALT).at(MAIL[0], 0, MAIL[1]));
add(box(38, 6, 28).material("metal", DARK).at(MAIL[0], 90, MAIL[1]));
// The floor lamp beside the sofa.
const LAMP = [1290, 590];
add(cylinder(3, 140, 6).material("metal", INK).at(LAMP[0], 0, LAMP[1]));
add(cone(16, 20, 12, 10).material("paper", "#F4EBDC").glow(0.5).at(LAMP[0], 140, LAMP[1]));

// The quiet corner in the back right, between the plant and the partitions.
// Retired agents rest here, and the app stands a small tombstone for each in
// rows on the plot declared in space() below: GRAVE_W wide and GRAVE_D deep,
// starting GRAVE_GAP in front of the graveyard station in the direction it
// faces (-z). The low wooden fence reaches above 25 cm, so agents walk
// around it. A memorial lamp stands behind the plot.
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
  add(box(x1 - x0, 1.5, z1 - z0).material("felt", "#4E5A48").at(cx, 0, (z0 + z1) / 2));
  // An axis-aligned bar from (ax, az) to (bx, bz), t thick and h high at y.
  const bar = (ax, az, bx, bz, y, h, t, finish, color) =>
    add(box(Math.abs(bx - ax) || t, h, Math.abs(bz - az) || t).material(finish, color).at((ax + bx) / 2, y, (az + bz) / 2));
  // A wooden frame and a low fence with a narrow gap at the front, too narrow to walk through.
  const gate = 12;
  bar(x0 - 3, z0, x1 + 3, z0, 0, 6, 6, "wood", WOOD);
  bar(x0 - 3, z1, x1 + 3, z1, 0, 6, 6, "wood", WOOD);
  bar(x0, z0, x0, z1, 0, 6, 6, "wood", WOOD);
  bar(x1, z0, x1, z1, 0, 6, 6, "wood", WOOD);
  for (const y of [16, 30]) {
    bar(x0, z0, x1, z0, y, 3, 3, "wood", OAK);
    bar(x0, z0, x0, z1, y, 3, 3, "wood", OAK);
    bar(x1, z0, x1, z1, y, 3, 3, "wood", OAK);
    bar(x0, z1, cx - gate, z1, y, 3, 3, "wood", OAK);
    bar(cx + gate, z1, x1, z1, y, 3, 3, "wood", OAK);
  }
  const posts = (from, to, at, alongX) => {
    const n = Math.max(1, Math.round(Math.abs(to - from) / 40));
    for (let i = 0; i <= n; i++) {
      const t = from + ((to - from) * i) / n;
      add(box(4, 36, 4).material("wood", WOOD).at(alongX ? t : at, 0, alongX ? at : t));
    }
  };
  posts(x0, x1, z0, true);
  posts(z0 + 40, z1 - 40, x0, false);
  posts(z0 + 40, z1 - 40, x1, false);
  posts(x0, cx - gate, z1, true);
  posts(cx + gate, x1, z1, true);
  // The memorial lamp behind the plot.
  add(cylinder(3, 110, 6).material("metal", INK).at(cx, 0, z0 - 50));
  add(cone(12, 16, 12, 8).material("paper", "#F4EBDC").glow(0.6).at(cx, 110, z0 - 50));
}

export default space({
  parts,
  walls: [
    { inside: [1, 0, 0], parts: leftWall },
    { inside: [0, 0, 1], parts: backWall },
  ],
  lights: [
    light([430, 260, 500], "#FFF1DC", 1.25, 840),
    light([lamp[0] - 16, 114, lamp[1] - 7], "#FFE2B0", 0.8, 220),
    light([770, 150, 120], "#7FE3D0", 0.25, 160),
    light([LAMP[0], 150, LAMP[1]], "#FFD9A0", 0.7, 340),
    light([GRAVE[0], 120, GRAVE[2] - GRAVE_GAP - GRAVE_D - 54], "#FFD9A0", 0.45, 220),
  ],
  ambient: { sky: "#2B3550", ground: "#1C1A18" },
  sun: { direction: [-0.35, -1, -0.45], color: "#BFD0FF", intensity: 0.3 },
  background: "#0B0D12",
  stations: {
    door: station([85, 0, DOOR_Z], { facing: 90 }),
    window: station([105, 0, WINDOW_Z + 15], { facing: 70 }),
    bookshelf: station([255, 0, 150], { facing: 30, tools: ["browser", "files"] }),
    router_cabinet: station([735, 0, 180], { facing: 120, tools: ["network", "system"] }),
    workbench: station([755, 0, BENCH[1]], { facing: 270, tools: ["terminal"] }),
    desk: station([320, 0, DESK[1] - 90], { facing: 20, tools: ["editor"] }),
    front_step: station([MAT[0], 0, MAT[1]], { facing: 45, front: true }),
    mailbox: station([MAIL[0] + 70, 0, MAIL[1] - 25], { facing: 290, tools: ["mail"] }),
    garden_bench: station([SOFA[0], 0, SOFA[1] + 85], { facing: 30 }),
    tree: station([PLANT[0] - 50, 0, PLANT[1] + 140], { facing: 45 }),
    graveyard: station(GRAVE, { facing: 180 }),
  },
  waypoints: {
    hall: [460, 0, 450],
    back: [580, 0, 250],
    entry: [520, 0, 860],
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
