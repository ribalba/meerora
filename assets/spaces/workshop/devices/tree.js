// The garden tree shows the free memory (D7). Full leaves while plenty of
// memory is available, thin from below 30 % and bare from below 10 %. The
// trunk and its raised bed are part of space.js, so agents walk around the
// tree either way.
//
// The device stands at the foot of the trunk.

const S = 1.2; // the tree of the bigger garden

const crown = (colors, scale) =>
  group(
    sphere(48 * scale, 1).material("foliage", colors[0]).at(0, 82, 0),
    sphere(34 * scale, 1).material("foliage", colors[1]).at(-30 * scale, 122 - 20 * (1 - scale), 18 * scale),
    sphere(30 * scale, 1).material("foliage", colors[2]).at(28 * scale, 128 - 24 * (1 - scale), -14 * scale),
  ).scale(S);

const full = crown(["#3E7A44", "#4F8A4A", "#46803F"], 1);
const thin = crown(["#8A9A48", "#8A9A48", "#8A9A48"], 0.68);

// Bare branches, and a few leaves on the ground.
const branch = (points) => tube(points, 1.6, 5).material("wood", "#5A3F2A");
const bare = group(
  branch([[0, 80, 0], [-22, 112, 10], [-30, 134, 16]]),
  branch([[0, 86, 0], [20, 116, -8], [26, 140, -14]]),
  branch([[0, 90, 0], [4, 124, 4], [2, 150, 2]]),
).scale(S);
const fallen = group(
  box(7, 0.6, 5).material("foliage", "#9A6A2E").rotate(0, 30, 0).at(-34, 29, 40),
  box(6, 0.6, 4).material("foliage", "#B07A34").rotate(0, -20, 0).at(38, 29, 24),
);
bare.add(fallen);

export default device({
  name: "Tree",
  params: {
    leaves: choice(["full", "thin", "bare"], "full").shows(full, "full").shows(thin, "thin").shows(bare, "bare"),
    free: text(""),
  },
  place: { at: [1060, 0, 300] },
  bind: {
    leaves: from("system.memory").steps([0.1, 0.3], ["bare", "thin", "full"]),
    free: from("system.memory").percent(),
  },
  status: {
    leaves: {
      full: "Healthy. {free} of the memory is free.",
      thin: "Memory is getting tight. {free} is free.",
      bare: "Almost no memory is free ({free}).",
    },
  },
  budget: { triangles: 1200 },
});
