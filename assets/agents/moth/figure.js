// Moth, a dark graphite cone with a round head, two faint sleepy eyes and
// curled antennae.

const GRAPHITE = "#3B404C";

const body = lathe(
  [
    [18, 0],
    [18.2, 1.4],
    [16.8, 20.9],
    [14, 37.8],
    [10.8, 50.6],
    [8.1, 59.4],
  ],
  14,
).material("matte", GRAPHITE);

const R = 13.5; // head radius
const head = sphere(R, 2).material("matte", "#474D5A").at(0, 58.4, 0);
const centre = head.center;

// Faint slit eyes that glow a little.
const eyeZ = Math.sqrt(R * R - 5 * 5) - 0.5;
const slit = capsule(0.9, 4.6, 6).rotate(0, 0, 90).scale(1, 1, 0.6).material("glass", "#9AA2B2").glow(0.35);
const eyePair = group(slit.at(-5, centre[1] - 0.9, eyeZ), slit.clone().at(5, centre[1] - 0.9, eyeZ));

// Antennae curling outwards from the top of the head.
const antenna = (s) =>
  tube(
    [
      [4.5 * s, centre[1] + 11.2, -1],
      [8.6 * s, centre[1] + 18.8, -2],
      [13.6 * s, centre[1] + 23, -3],
      [18.9 * s, centre[1] + 24.3, -3.5],
    ],
    0.7,
    4,
  ).material("matte", "#5C6372");
const antennae = group(antenna(-1), antenna(1));

export default figure({
  parts: [body, head, eyePair, antennae],
  anchors: { ring: head.top.above(20), face: eyePair },
  animations: {
    working: bob(0.8, 2.2),
    waiting: hop(1, 8).then(face("viewer")),
    resting: breathe(6, 0.025).eyes("closed"),
  },
  budget: { triangles: 3000 },
});
