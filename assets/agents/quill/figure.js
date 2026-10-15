// Quill, an ivory bell shaped like a chess piece, with a round head, two dark
// eyes and a small crown.

const IVORY = "#EFE8DA";

const body = lathe(
  [
    [20, 0],
    [20.6, 1.6],
    [19.3, 18.4],
    [16.2, 33.4],
    [12.6, 45],
    [9.5, 53.1],
  ],
  18,
).material("glaze", IVORY);
const collar = cylinder(10.4, 2.4, 18).material("glaze", "#D6CAB4").at(0, 52.3, 0);

const R = 13.4; // head radius
const head = sphere(R, 2).material("glaze", "#F7F2E8").at(0, 52.5, 0);
const centre = head.center;

// Tall dark eyes, slightly flattened against the head.
const eyeY = centre[1] + 0.4;
const eyeZ = Math.sqrt(R * R - 4.7 * 4.7) - 0.9;
const eye = capsule(1.6, 7.1, 6).scale(1, 1, 0.5).material("glaze", "#1F2A44");
const eyePair = group(eye.at(-4.7, eyeY - 3.55, eyeZ), eye.clone().at(4.7, eyeY - 3.55, eyeZ));

// Small crown on top: a hollow band with five flat points and brass tips.
const crownY = head.top[1] - 3.2;
const band = lathe(
  [
    [10.2, 0],
    [10.9, 3.4],
    [9.9, 3.4],
    [9.3, 0.4],
  ],
  15,
)
  .material("glaze", "#FFFFFF")
  .at(0, crownY, 0);
const rim = torus(10.3, 0.6, 15, 4).material("brass", "#CDBF9F").at(0, crownY - 0.6, 0);
const crown = group(band, rim);
for (let i = 0; i < 5; i++) {
  const deg = i * 72;
  const a = (deg * Math.PI) / 180;
  const at = [10.2 * Math.sin(a), crownY + 2.8, 10.2 * Math.cos(a)];
  const spike = extrude(
    [
      [-3.8, 0],
      [3.8, 0],
      [0, 8],
    ],
    1,
  ).material("glaze", "#FFFFFF");
  const tip = sphere(1, 0).material("brass", "#CDBF9F").at(0, 7.2, 0);
  crown.add(group(spike, tip).rotate(0, deg, 0).at(at));
}

export default figure({
  parts: [body, collar, head, eyePair, crown],
  anchors: { ring: crown.top.above(14), face: eyePair },
  animations: {
    working: bob(0.8, 1.6),
    waiting: hop(2, 8).then(face("viewer")),
    resting: breathe(4).eyes("closed"),
  },
  budget: { triangles: 3000 },
});
