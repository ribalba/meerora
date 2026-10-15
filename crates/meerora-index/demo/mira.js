// Mira, the guide agent of Meridian Software, a fictional company. A teal
// body, a white round head and the Meridian mark on the chest, as in screen 4.
// External figures get a smaller budget than local ones.

const TEAL = "#1F6F6B";
const MINT = "#7FE3D0";
const WHITE = "#EEF2F0";

const base = cylinder(17, 4, 16).material("stone", "#163F3D");
const body = capsule(14, 56, 14).material("enamel", TEAL).on(base);
const collar = torus(11.5, 1.6, 16, 6).material("enamel", MINT).glow(0.35).at(0, body.top[1] - 7, 0);
const head = sphere(13, 2).material("ceramic", WHITE).above(body, -5);

const hy = head.center[1];
const eyeShape = capsule(1.6, 6.5, 6).scale(1, 1, 0.6).material("glaze", "#14302E");
const eyePair = group(eyeShape.at(-4.4, hy - 2.5, 12), eyeShape.clone().at(4.4, hy - 2.5, 12));

// The mark: a ring with a slanted bar.
const ring = torus(5, 1, 16, 4).rotate(90, 0, 0).material("enamel", WHITE).at(0, 27, 14.4);
const bar = box(12, 1.6, 1.4).material("enamel", WHITE).at(0, 32.2, 15);

const arm = capsule(3, 20, 8).material("enamel", TEAL);
const arms = group(arm.rotate(0, 0, 12).at(-15.5, 18, 0), arm.clone().rotate(0, 0, -12).at(15.5, 18, 0));

export default figure({
  parts: [base, body, collar, head, eyePair, ring, bar, arms],
  anchors: { ring: head.top.above(12), face: eyePair },
  animations: {
    working: bob(1.2, 1.6),
    waiting: hop(1, 6).then(face("viewer")),
    resting: breathe(4).eyes("closed"),
  },
  budget: { triangles: 1500 },
});
