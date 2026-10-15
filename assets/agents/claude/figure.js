// Claude, shown as a floating faceted gem above a small dark disc. This is a
// placeholder and deliberately not the Anthropic logo.

const LAVENDER = "#B7A6FF";
const VIOLET = "#9B8CF0";

const disc = cylinder(12, 2.4, 18).material("stone", "#23202F");
const halo = torus(8.5, 0.7, 18, 4).material("glass", LAVENDER).glow(0.5).at(0, 2.2, 0);

// A six-sided crystal: a point below, a short upright belt and a taller point
// above. Turned by 30 degrees so a flat facet looks at the viewer.
const lower = lathe(
  [
    [0, 0],
    [15, 20],
  ],
  6,
)
  .material("glass", VIOLET)
  .glow(0.25);
const upper = lathe(
  [
    [15, 0],
    [15, 7],
    [0, 30],
  ],
  6,
)
  .material("glass", LAVENDER)
  .glow(0.25)
  .on(lower);
const gem = group(lower, upper).rotate(0, 30, 0).at(0, 26, 0);

// Two dark eyes on the front facet of the belt.
const belt = upper.bottom[1] + 3.5;
const facet = 15 * Math.cos(Math.PI / 6);
const eye = capsule(1.2, 5.2, 6).scale(1, 1, 0.5).material("glaze", "#2A2546");
const eyePair = group(eye.at(-3.4, belt - 2.6, facet), eye.clone().at(3.4, belt - 2.6, facet));

// Two small shards keep it company.
const shard = lathe(
  [
    [0, 0],
    [2.6, 4],
    [0, 9],
  ],
  4,
)
  .material("glass", VIOLET)
  .glow(0.4);
const shards = group(shard.rotate(0, 0, 18).at(-21, 44, -3), shard.clone().rotate(0, 0, -36).at(21, 36, 2));

export default figure({
  parts: [disc, halo, gem, eyePair, shards],
  anchors: { ring: gem.top.above(12), face: eyePair },
  animations: {
    working: spin(6),
    waiting: hop(1, 5).then(face("viewer")),
    resting: breathe(5).eyes("closed"),
  },
  budget: { triangles: 3000 },
});
