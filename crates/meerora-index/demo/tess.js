// Tess, the ferry guide of Tidewater Ferries, a fictional company. A navy
// coat, a captain's cap and a life ring around the waist.

const NAVY = "#2B4C7E";
const INK = "#1D2433";

const body = cone(18, 46, 12, 11).material("felt", NAVY);
const buttons = [14, 22].map((y) => sphere(1.3, 1).material("brass", "#D9A441").at(0, y, 15.4));
const lifeRing = torus(16.5, 2.6, 16, 6).material("plastic", "#E8743B").at(0, 28, 0);
const head = sphere(12, 2).material("ceramic", "#F1D9C0").above(body, -2);

const hy = head.center[1];
const eyeDot = sphere(1.7, 1).material("glaze", INK);
const eyePair = group(eyeDot.at(-4, hy - 1, 10.6), eyeDot.clone().at(4, hy - 1, 10.6));

const cap = cylinder(12.6, 6, 14).material("felt", "#F4F1EA").above(head, -7);
const band = cylinder(12.8, 1.6, 14).material("felt", INK).at(0, cap.bottom[1], 0);
const brim = cylinder(7, 1.2, 10).scale(1, 1, 0.7).material("plastic", INK).at(0, cap.bottom[1], 9);
const badge = emblem("star", { size: 4, color: "#F2B45A" }).at(0, cap.bottom[1] + 2, 12.8);

export default figure({
  parts: [body, ...buttons, lifeRing, head, eyePair, cap, band, brim, badge],
  anchors: { ring: cap.top.above(12), face: eyePair },
  animations: {
    working: wave(1.8, 4),
    waiting: hop(2, 8).then(face("viewer")),
    resting: breathe(4.5).eyes("closed"),
  },
  budget: { triangles: 1500 },
});
