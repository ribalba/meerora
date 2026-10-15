// The shuttle on the viewport ledge (D7) stands for airplane mode. It
// follows airplane mode both ways: while every radio is off it leaves the
// ledge and circles the module, and a click on its card switches airplane
// mode once you allowed the capability (D8).
//
// The device stands on the ledge, so local axes are the axes of the module.
// It hangs on the left wall and is cut away together with it.

const HULL = "#E8ECF2";
const DARK = "#3A4250";

// A small shuttle with its nose at +z, centred on the origin, keel at y = 0.
function shuttle(glow = 0) {
  const body = extrude([[0, 30], [9, 4], [9, -24], [-9, -24], [-9, 4]], 10).rotate(90, 0, 0).material("enamel", HULL);
  const wings = extrude([[0, 6], [22, -20], [-22, -20]], 1.4).rotate(90, 0, 0).material("enamel", HULL).at(0, 1, -6);
  const cockpit = box(8, 3, 9).material("glass", "#8CD3FF").glow(0.6).at(0, 10, 16);
  const engine = box(12, 6, 2).material("glass", "#F2B45A").glow(glow).at(0, 2, -25);
  return group(body.at(0, 0, 3), wings, cockpit, engine);
}

const parked = shuttle(0.1).rotate(0, 15, 0).at(6, 0, 0);

// In flight it circles the module around `centre` and leaves a trail of
// engine light behind it.
const centre = [250, 110, 160];
const R = 120;
const flying = shuttle(1).rotate(-10, 180, 8).at(centre[0] + R, centre[1], centre[2]);
const trail = [];
for (let i = 1; i <= 10; i++) {
  const a = (-8 * i * Math.PI) / 180;
  trail.push(
    box(2.4, 2.4, 10 - i * 0.6)
      .material("glass", "#F2B45A")
      .glow(1 - i * 0.07)
      .rotate(0, (a * 180) / Math.PI, 0)
      .at(centre[0] + R * Math.cos(a), centre[1] + 2, centre[2] - R * Math.sin(a)),
  );
}
const inFlight = group(flying, ...trail);

export default device({
  name: "Airplane mode",
  params: {
    flying: toggle(false).shows(parked, false).shows(inFlight, true, { pivot: centre }).animate(spin(8)),
  },
  place: { at: [13, 98, 334], wall: [1, 0, 0] },
  bind: { flying: "network.airplane_mode" },
  actions: { toggle: action("network.airplane_mode.write", "toggle", "Toggle airplane mode") },
  status: { flying: { true: "On. The shuttle circles the module.", false: "Off. The shuttle is docked at the viewport." } },
  budget: { triangles: 800 },
});
