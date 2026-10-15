// The paper plane on the office window sill (D7). It follows airplane
// mode both ways. Switching airplane mode anywhere makes it take off, and a
// click on its card switches airplane mode once you allowed the capability
// (D8).
//
// The device stands on the sill, so local axes are the axes of the space.
// It hangs on the left wall and is cut away together with it.

const L = 52; // length, nose to tail, a little larger than paper so it reads from afar
const W = 36; // wingspan
const PAPER = "#F4F1EA";
const FOLD = "#C9C1B0";

// A paper plane with its nose at +z, centred on the origin, keel at y = 0.
function plane(glow = 0) {
  // A wing is a flat triangle from the nose back to the tail, folded up a little.
  const wing = (side) =>
    extrude([[0, L / 2], [(side * W) / 2, -L / 2], [0, -L / 2]], 0.6)
      .rotate(90, 0, 0)
      .material("paper", PAPER)
      .glow(glow);
  const left = wing(-1).rotate(0, 0, -8);
  const right = wing(1).rotate(0, 0, 8);
  // The keel is the folded body under the wings.
  const keel = extrude([[L / 2, 0], [-L / 2, 0], [-L / 2, -4]], 0.6).rotate(0, -90, 0).material("paper", FOLD);
  keel.at(0, 0, 0);
  left.at(-W / 4, 3.4, 0);
  right.at(W / 4, 3.4, 0);
  return group(keel, left, right);
}

// Parked on the sill, nose along the sill and turned a little into the room.
const parked = plane().rotate(0, 10, 0).at(4, 0, 0);

// In the air it circles the room around `centre`, nose up and banked into
// the turn, and leaves a dotted trail that fades behind it.
const centre = [250, 100, 160];
const R = 120;
const flying = plane(0.2).rotate(-14, 180, 6).at(centre[0] + R, centre[1], centre[2]);
const trail = [];
for (let i = 1; i <= 12; i++) {
  const a = (-8 * i * Math.PI) / 180; // behind the plane, the circle runs towards -z
  const dash = box(2, 2, 9 - i * 0.5)
    .material("glass", "#E8F4FF")
    .glow(1 - i * 0.05)
    .rotate(0, (a * 180) / Math.PI, 0)
    .at(centre[0] + R * Math.cos(a), centre[1] + 2 - i * 1.4, centre[2] - R * Math.sin(a));
  trail.push(dash);
}
const inTheAir = group(flying, ...trail);

export default device({
  name: "Airplane mode",
  params: {
    flying: toggle(false).shows(parked, false).shows(inTheAir, true, { pivot: centre }).animate(spin(7)),
  },
  place: { at: [11, 91, 334], wall: [1, 0, 0] },
  bind: { flying: "network.airplane_mode" },
  actions: { toggle: action("network.airplane_mode.write", "toggle", "Toggle airplane mode") },
  status: { flying: { true: "On. The plane circles the room.", false: "Off. The plane is parked on the window sill." } },
  budget: { triangles: 600 },
});
