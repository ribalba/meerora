// Rivet, meerora's helper and system agent. A small enamel robot on treads
// with a screen for a face, a wrench arm and a bent antenna. Ported from the
// robot script in BRIEF section 5.

const ENAMEL = "#E9E4D8";
const TRIM = "#9A927F";
const STEEL = "#8A8F9A";

const base = treads({ wheels: 3, width: 46 });

const body = box(40, 46, 32).material("enamel", ENAMEL).bevel(5).on(base);
body.add(
  vents(3).scale(0.8).at(body.front.offset(-8, 10, 0)),
  dial().scale(0.85).at(body.front.offset(9, 6, 0)),
  emblem("lock", { size: 8 }).at(body.front.offset(0, -13, 0.3)),
);

// Neck of three stacked rings, as in the mockup.
const neck = group(
  cylinder(3.8, 7, 8).material("metal", STEEL),
  ...[0.3, 2.6, 4.9].map((y) => cylinder(5.2, 1.6, 10).material("enamel", TRIM).move(0, y, 0)),
).on(body);

const head = box(40, 28, 30).material("enamel", ENAMEL).bevel(4).above(neck, -0.6);
const display = screen(31, 18, "#1E2A2E").at(head.front.offset(0, -9, 0));
const glyph = emblem("wifi", { size: 12 }).at(display.front.offset(0, -6, 0));
const bolt = cylinder(2.6, 2, 8).rotate(0, 0, 90).material("metal", TRIM);
const ears = group(bolt.at(head.left.offset(-1, -2.6, 0)), bolt.clone().at(head.right.offset(1, -2.6, 0)));

// Long arm with the wrench on the left, short claw arm on the right.
const shoulderL = body.left.offset(0.5, 15, 2);
const wristL = shoulderL.offset(-10, -22, 7);
const armL = tube([shoulderL, shoulderL.offset(-8, -10, 3), wristL], 2.1, 6).material("metal", STEEL);
const tool = wrench(10).rotate(180, 0, 0).below(wristL, -1);
const shoulderR = body.right.offset(-0.5, 15, 2);
const handR = shoulderR.offset(8, -10, 2);
const armR = group(
  tube([shoulderR, handR], 2.1, 6),
  tube([handR, handR.offset(4.5, -1, 0)], 1.1, 5),
  tube([handR, handR.offset(1.5, -4.5, 0)], 1.1, 5),
).material("metal", STEEL);

const ant = bentRod(13, { bend: 45 }).on(head, { x: 9 });
const lamp = bulb("#F2B45A", 3.2).at(ant.tip.offset(0, -3.2, 0));

// Cable tail from the back of the body to the floor.
const back = body.back;
const tail = tube(
  [back.offset(0, -10, 0.5), back.offset(0, -14, -5), back.offset(2, -24, -9), [5, 1.4, back[2] - 12], [12, 1.4, back[2] - 15]],
  1.4,
  6,
).material("plastic", "#3B404C");

export default figure({
  parts: [base, body, neck, head, display, glyph, ears, armL, tool, armR, ant, lamp, tail],
  anchors: { ring: lamp.top.above(8), face: display },
  animations: {
    working: bob(0.8, 0.9),
    waiting: hop(2, 8).then(face("viewer")),
    resting: breathe(5, 0.015).eyes("closed"),
  },
  budget: { triangles: 3000 },
});
