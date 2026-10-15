// Alex, the personal agent of Alex Example, a placeholder person. A coral
// sweater with a stripe, and a small sail for the weekends.

const CORAL = "#E07A5F";

const body = egg(30, 50, 14).material("felt", CORAL);
const stripe = torus(14.4, 1.4, 16, 4).material("felt", "#F4F1EA").at(0, 21, 0);
const head = sphere(11, 2).material("ceramic", "#F2D2B6").above(body, -3);
const hair = sphere(11.6, 2).scale(1, 0.55, 1).material("felt", "#3B2F2F").above(head, -9).move(0, 0, -1.5);

const hy = head.center[1];
const eyeDot = sphere(1.6, 1).material("glaze", "#2A2320");
const eyePair = group(eyeDot.at(-3.8, hy - 1.5, 9.8), eyeDot.clone().at(3.8, hy - 1.5, 9.8));

const mast = cylinder(0.8, 28, 6).material("wood", "#8A6A4A").at(19, 6, 4);
const sail = extrude([[0, 0], [10, 4], [0, 17]], 0.6).material("paper", "#F4F1EA").at(24.5, 14, 4);
const boat = group(mast, sail);

export default figure({
  parts: [body, stripe, head, hair, eyePair, boat],
  anchors: { ring: hair.top.above(12), face: eyePair },
  animations: {
    working: bob(1.5, 1.3),
    waiting: hop(1, 10).then(face("viewer")),
    resting: breathe(4).eyes("closed"),
  },
  budget: { triangles: 1500 },
});
