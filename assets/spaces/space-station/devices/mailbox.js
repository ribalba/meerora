// The beacon of the comms box (D7). Its mast rises and its light rocks
// while Wren, the mail agent, holds up a letter (its `letter` param, D24).
// The comms box itself is part of space.js.
//
// The device stands on the lid of the box.

const DARK = "#2A2F3B";

const up = group(
  cylinder(1.4, 34, 6).material("metal", DARK).at(8, 0, 0),
  sphere(4, 1).material("glass", "#E0675A").glow(1).at(8, 34, 0),
);
const down = group(cylinder(1.4, 6, 6).material("metal", DARK).at(8, 0, 0), sphere(2.4, 0).material("glass", "#5A3030").at(8, 6, 0));

export default device({
  name: "Comms box",
  params: {
    up: toggle(false).shows(up, true, { pivot: [8, 0, 0] }).animate(wave(1.2, 9)).shows(down, false),
  },
  place: { at: [975, 118, 1000] },
  bind: { up: "agent.wren.param.letter" },
  status: { up: { true: "Wren has a message for you.", false: "No new messages." } },
  budget: { triangles: 300 },
});
