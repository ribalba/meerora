// The flag of the garden mailbox (D7). It goes up while Wren, the mail
// agent, holds up a letter (its `letter` param, D24), and waves so you notice
// it. The mailbox itself is part of space.js.
//
// The device stands at the foot of the flag, on the right side of the box.

const RED = "#E0675A";
const POLE = "#8A8F99";

const up = group(box(3, 22, 3).material("metal", POLE).at(0, 0, 0), box(12, 8, 2).material("enamel", RED).at(7.5, 14, 0));
const down = group(box(22, 3, 3).material("metal", POLE).at(11, 0, 0), box(2, 10, 8).material("enamel", RED).at(21, -8, 0));

export default device({
  name: "Mailbox",
  params: {
    up: toggle(false).shows(up, true, { pivot: [0, 0, 0] }).animate(wave(1.6, 7)).shows(down, false),
  },
  place: { at: [995, 100, 1006] },
  bind: { up: "agent.wren.param.letter" },
  status: { up: { true: "Wren holds up a letter for you.", false: "No new mail." } },
  budget: { triangles: 200 },
});
