// Wren, the mail agent from screen 3. An ochre felt egg with a postal cap and
// a satchel. When it finds important mail it switches `letter` on and raises
// an envelope.

const W = 52; // egg width
const H = 78; // egg height
const Y0 = 3; // the egg rests on its feet

// Radius of the egg at height y, following the curve egg() uses.
const K = 0.25;
const C = (Math.sqrt(1 + 8 * K * K) - 1) / (4 * K);
const PEAK = Math.sqrt(1 - C * C) * (1 + K * C);
function radiusAt(y) {
  const c = Math.min(1, Math.max(-1, 1 - (2 * (y - Y0)) / H));
  return ((W / 2) * Math.sqrt(1 - c * c) * (1 + K * c)) / PEAK;
}
// A point on the egg surface at height y, `deg` degrees from the front
// towards +x, `out` cm above the surface.
function onEgg(y, deg, out = 0) {
  const a = (deg * Math.PI) / 180;
  const r = radiusAt(y) * 0.985 + out;
  return [r * Math.sin(a), y, r * Math.cos(a)];
}

const body = egg(W, H, 16).material("felt", "#D9A441").at(0, Y0, 0);

// Stitched seam down the front, a little to the left.
const seamPoints = [];
for (let y = Y0 + 2; y <= Y0 + H - 2; y += (H - 4) / 8) {
  const t = (y - Y0) / H;
  seamPoints.push(onEgg(y, -14 * Math.sin(Math.PI * t), 0.2));
}
const seam = tube(seamPoints, 0.5, 4).material("felt", "#A9782A");

// The visor is a ring around an axis moved forward, so it shows only on the
// front and its back half stays inside the egg.
function frontBand(y0, y1, halfWidth, out) {
  const ym = (y0 + y1) / 2;
  const re = radiusAt(ym);
  const k = re + out - Math.sqrt(re * re - halfWidth * halfWidth);
  const ringR = (k * k + halfWidth * halfWidth) / (2 * k);
  const shift = re + out - ringR;
  const profile = [];
  for (let i = 0; i <= 4; i++) {
    const y = y0 + ((y1 - y0) * i) / 4;
    const bulge = 0.35 + 0.65 * Math.sin((Math.PI * i) / 4);
    profile.push([radiusAt(y) + out * bulge - shift, y - y0]);
  }
  return lathe(profile, 28).at(0, y0, shift);
}
const visor = frontBand(47, 62, 16, 1.6).material("glaze", "#2A1C0C");
const eyePair = eyes(13, 3, "#FFE7B0").at(0, 54.5 - 3, radiusAt(54.5) + 1.6);

// Blue postal cap with an amber badge.
const brim = cylinder(14, 1.4, 18).material("felt", "#2F4E7E").at(0, 78.5, 2.5);
const crown = lathe(
  [
    [11, 0],
    [10.6, 8],
    [9.6, 11],
    [6, 12.6],
    [0, 13],
  ],
  16,
)
  .material("felt", "#4466A0")
  .on(brim, { z: -2.5 });
const badge = cylinder(2.2, 1, 10).rotate(90, 0, 0).material("brass", "#F2B45A").at(0, crown.bottom[1] + 4.3, 10.6);

// Leather satchel on the right hip, carried on a strap over the left shoulder.
const satchel = box(20, 16, 7).bevel(1.5).material("leather", "#8A5A3E").rotate(0, 32, 0).at(19.5, 13, 16);
const flap = box(20.6, 6, 7.6).bevel(1.2).material("leather", "#A06A48").rotate(0, 32, 0).at(19.5, 23.5, 16.1);
const buckle = box(3, 2.4, 1).material("brass", "#C8A050").rotate(0, 32, 0).at(21.4, 22.6, 19.6);
const strap = tube(
  [
    onEgg(29, 40, 1.1),
    onEgg(36, 12, 1.1),
    onEgg(43, -20, 1.1),
    onEgg(50, -52, 1.1),
    onEgg(56, -90, 1.1),
    onEgg(55, -130, 1.1),
    onEgg(46, -165, 1.1),
    onEgg(36, 160, 1.1),
    onEgg(29, 120, 1.1),
  ],
  1.3,
  4,
).material("leather", "#6E4B2A");

// Stubby feet.
const foot = capsule(4.2, 15, 8).rotate(90, 0, 0).scale(1.1, 0.8, 1).material("leather", "#5A3F1E");
const feet = group(foot.at(-10, 0, 2), foot.clone().at(10, 0, 2));

// The raised arm with a letter, shown only while `letter` is on.
const shoulder = [-21, 38, 9];
const arm = tube(
  [
    shoulder,
    [-27.5, 46, 10.5],
    [-31, 55, 11.5],
    [-32, 62, 12],
  ],
  2.5,
  6,
).material("felt", "#C8932F");
const hand = sphere(3, 1).material("felt", "#C8932F").at(-32, 60, 12);
const envelope = box(24, 16, 1.2).material("paper", "#F4F1EA");
const fold = group(
  box(14.2, 0.7, 0.4).material("paper", "#C9C2B2").rotate(0, 0, -36.9).at(-6, 6.95, 0.75),
  box(14.2, 0.7, 0.4).material("paper", "#C9C2B2").rotate(0, 0, 36.9).at(6, 6.95, 0.75),
);
const seal = cylinder(3, 0.8, 12).rotate(90, 0, 0).material("glaze", "#E0675A").at(0, 3.5, 1.1);
const letter = group(envelope, fold, seal).rotate(0, 0, 10).at(-34, 64, 12);
const raisedArm = group(arm, hand, letter);

export default figure({
  parts: [feet, body, seam, visor, eyePair, brim, crown, badge, strap, satchel, flap, buckle],
  anchors: { ring: crown.top.above(14), face: eyePair },
  animations: {
    working: bob(0.6),
    waiting: hop(2).then(face("viewer")),
    resting: breathe(4).eyes("closed"),
  },
  // States the agent may set at runtime.
  params: {
    letter: toggle(false).shows(raisedArm, true, { pivot: shoulder }).animate(wave(1.5)),
  },
  budget: { triangles: 3000 },
});
