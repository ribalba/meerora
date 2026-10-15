// Ember, a copper ceramic capsule with a dark visor, two amber eyes and a
// short antenna.

const R = 21; // body radius
const H = 82; // body height

// Radius of the capsule at height y.
function radiusAt(y) {
  const d = Math.max(0, y - (H - R), R - y);
  return Math.sqrt(Math.max(0, R * R - d * d));
}

const body = capsule(R, H, 20).material("ceramic", "#C7784A");

// The visor is a ring around an axis moved forward, so it shows only on the
// front and its back half stays inside the body.
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
const visor = frontBand(49, 63, 15.5, 1.6).material("glaze", "#2A1810");
const eyePair = eyes(12, 2.8, "#FFC870").at(0, 56 - 2.8, radiusAt(56) + 1.6);

const ant = bentRod(11, { bend: 0, r: 1, color: "#3A2A20" }).rotate(0, 0, -25).on(body, { x: 5 }).move(0, -1.6, 0);
const lamp = bulb("#FFC870", 3.5).at(ant.tip.offset(0, -3.5, 0));

export default figure({
  parts: [body, visor, eyePair, ant, lamp],
  anchors: { ring: lamp.top.above(10), face: eyePair },
  animations: {
    working: bob(0.8, 1.2),
    waiting: hop(2, 10).then(face("viewer")),
    resting: breathe(4.5).eyes("closed"),
  },
  budget: { triangles: 3000 },
});
