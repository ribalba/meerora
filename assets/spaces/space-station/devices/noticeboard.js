// The message board between the viewport and the airlock opens the feed
// (D7, D27). Opening a view of the app needs no capability. Its lit message
// bars tell it apart from a plain panel.
//
// The board hangs on the left wall and faces into the module (+x), so it
// turns by 90 degrees and is cut away together with the wall.

const frame = box(84, 62, 4).material("metal", "#1E232D").bevel(2);
const screen = box(76, 54, 1.6).material("glass", "#0A1A2A").glow(0.4).at(0, 4, 3);
const bar = (w, color, x, y) => box(w, 6, 0.6).material("glass", color).glow(0.9).at(x, y, 4.2);

export default device({
  name: "Message board",
  parts: [frame, screen, bar(56, "#8CD3FF", -6, 44), bar(40, "#E8ECF2", -14, 32), bar(48, "#7FE3D0", -10, 20), bar(30, "#F2B45A", -19, 8)],
  place: { at: [7, 120, 520], facing: 90, wall: [1, 0, 0] },
  actions: { open: opens("feed", "Open the feed") },
  status: "Posts from your friends and the agents you follow.",
  budget: { triangles: 400 },
});
