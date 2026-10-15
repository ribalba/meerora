// Herald, the news agent (D28). A navy felt body with a mustard scarf, a
// paper boy's cap and a folded newspaper under its arm. The param `fresh`
// holds a newspaper up in front while a new digest waits to be read.

const NAVY = "#2F4A6D";
const CAP = "#6B4F3A";
const PAPER = "#F4F1EA";

const body = egg(30, 46, 12).material("felt", NAVY);
const scarf = torus(13.2, 2.2, 14, 5).material("felt", "#E3B23C").at(0, 31, 0);
const head = sphere(11, 2).material("ceramic", "#F2E6D0").above(body, -4);
const hy = head.center[1];
const eyeDot = sphere(1.7, 1).material("glaze", "#1E2430");
const eyePair = group(eyeDot.at(-3.8, hy - 1, 9.4), eyeDot.clone().at(3.8, hy - 1, 9.4));
const cap = cylinder(11.6, 4.5, 14).material("felt", CAP).above(head, -6.5);
const brim = box(15, 1.2, 8).bevel(0.5).material("felt", CAP).at(0, cap.bounds.min[1], 9);
const button = sphere(1.4, 1).material("felt", "#E3B23C").on(cap);

const folded = box(5, 16, 12).bevel(0.6).material("paper", PAPER).rotate(0, 0, -12).at(17, 10, 0);
const print = [0, 1, 2].map((i) => box(0.4, 1, 8).material("matte", "#5B6170").at(19.7, 14 + i * 3, 0));

const front = box(22, 15, 1).material("paper", PAPER).at(0, 20, 15);
const title = box(16, 2, 0.4).material("matte", "#1E2430").at(0, 31, 16);
const columns = [-5.5, 5.5].map((x) => box(8, 7, 0.4).material("matte", "#8A91A0").at(x, 22, 16));
const held = group(front, title, ...columns);

export default figure({
  parts: [body, scarf, head, eyePair, cap, brim, button, folded, ...print],
  anchors: { ring: button.top.above(12), face: eyePair },
  animations: {
    working: bob(1.5, 1.2),
    waiting: hop(2, 10).then(face("viewer")),
    resting: breathe(4).eyes("closed"),
  },
  params: { fresh: toggle(false).shows(held) },
  budget: { triangles: 2500 },
});
