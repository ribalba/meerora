// The hydroponic plant in the bay shows the free memory (D7). Full leaves
// while plenty of memory is available, thin from below 30 % and bare from
// below 10 %. The planter, its soil and the stalk are part of space.js, so
// agents walk around the plant either way.
//
// The device stands on the soil at the foot of the stalk.

const leaf = (color, size, y, turn, tilt) =>
  extrude([[0, 0], [size * 0.45, size * 0.5], [0, size], [-size * 0.45, size * 0.5]], 0.8)
    .material("foliage", color)
    .rotate(tilt, turn, 0)
    .at(Math.sin((turn * Math.PI) / 180) * size * 0.3, y, Math.cos((turn * Math.PI) / 180) * size * 0.3);

const crown = (colors, size, count) => {
  const leaves = [];
  for (let i = 0; i < count; i++) {
    leaves.push(leaf(colors[i % colors.length], size, 18 + (i % 4) * 14, i * (360 / count), 35 + (i % 3) * 12));
  }
  leaves.push(sphere(size * 0.35, 1).material("foliage", colors[0]).at(0, 66, 0));
  return group(...leaves);
};

const full = crown(["#4F9A4A", "#5FAE52", "#3E8A44"], 48, 14);
const thin = crown(["#8A9A48", "#9AA850"], 30, 6);
const bare = group(
  leaf("#8C6A2E", 12, 6, 40, 80),
  leaf("#9A7A34", 10, 4, 200, 85),
  sphere(3, 0).material("foliage", "#6B5A2E").at(0, 70, 0),
);

export default device({
  name: "Hydroponics",
  params: {
    leaves: choice(["full", "thin", "bare"], "full").shows(full, "full").shows(thin, "thin").shows(bare, "bare"),
    free: text(""),
  },
  place: { at: [1060, 31, 300] },
  bind: {
    leaves: from("system.memory").steps([0.1, 0.3], ["bare", "thin", "full"]),
    free: from("system.memory").percent(),
  },
  status: {
    leaves: {
      full: "Thriving. {free} of the memory is free.",
      thin: "Memory is getting tight. {free} is free.",
      bare: "Almost no memory is free ({free}).",
    },
  },
  budget: { triangles: 1200 },
});
