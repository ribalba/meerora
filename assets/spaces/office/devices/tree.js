// The large plant in the lounge shows the free memory (D7). Full leaves
// while plenty of memory is available, thin from below 30 % and bare from
// below 10 %. The pot and the stem are part of space.js, so agents walk
// around the plant either way.
//
// The device stands on the soil at the foot of the stem.

const cluster = (colors, scale) =>
  group(
    sphere(30 * scale, 1).material("foliage", colors[0]).at(0, 46, 0),
    sphere(24 * scale, 1).material("foliage", colors[1]).at(-22 * scale, 74 - 10 * (1 - scale), 10 * scale),
    sphere(22 * scale, 1).material("foliage", colors[2]).at(20 * scale, 80 - 12 * (1 - scale), -8 * scale),
    sphere(18 * scale, 1).material("foliage", colors[1]).at(4 * scale, 104 - 16 * (1 - scale), 6 * scale),
  );

const full = cluster(["#3E7A44", "#4F8A4A", "#46803F"], 1);
const thin = cluster(["#8A9A48", "#8A9A48", "#9AA850"], 0.62);
const twig = (points) => tube(points, 1.2, 5).material("wood", "#5A3F2A");
const bare = group(
  twig([[0, 58, 0], [-16, 84, 6], [-20, 98, 10]]),
  twig([[0, 60, 0], [14, 86, -6], [18, 104, -8]]),
  box(6, 0.6, 4).material("foliage", "#9A6A2E").rotate(0, 30, 0).at(-14, 0.5, 18),
);

export default device({
  name: "Plant",
  params: {
    leaves: choice(["full", "thin", "bare"], "full").shows(full, "full").shows(thin, "thin").shows(bare, "bare"),
    free: text(""),
  },
  place: { at: [1060, 53, 300] },
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
