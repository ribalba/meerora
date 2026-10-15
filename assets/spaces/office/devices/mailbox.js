// The letter in the mail tray (D7). It stands up and rocks while Wren, the
// mail agent, holds up a letter (its `letter` param, D24), and lies flat in
// the tray otherwise. The tray and its cabinet are part of space.js.
//
// The device stands on the floor of the tray.

const PAPER = "#F4F1EA";
const SEAL = "#E0675A";

const up = group(
  box(24, 16, 0.8).material("paper", PAPER).at(0, 0, 0),
  extrude([[-12, 16], [12, 16], [0, 8]], 0.6).material("paper", "#DCD6C8").move(0, 0, 0.5),
  sphere(1.6, 0).material("enamel", SEAL).at(0, 7, 0.8),
);
const down = box(24, 0.8, 16).material("paper", PAPER).at(0, 0, 0);

export default device({
  name: "Mail tray",
  params: {
    up: toggle(false).shows(up, true, { pivot: [0, 0, 0] }).animate(wave(1.6, 7)).shows(down, false),
  },
  place: { at: [975, 97, 1000] },
  bind: { up: "agent.wren.param.letter" },
  status: { up: { true: "Wren holds up a letter for you.", false: "No new mail." } },
  budget: { triangles: 200 },
});
