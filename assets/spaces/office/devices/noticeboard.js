// A noticeboard between the window and the door that opens the feed (D7,
// D27). Opening a view of the app needs no capability. The notes on it tell
// it apart from a plain board.
//
// The board hangs on the left wall and faces into the office (+x), so it
// turns by 90 degrees and is cut away together with the wall.

const frame = box(84, 60, 3).material("metal", "#9AA1B0");
const cork = box(78, 54, 1.6).material("felt", "#B98A57").at(0, 3, 2);
const note = (w, h, color, x, y, tilt) => box(w, h, 0.6).material("paper", color).rotate(0, 0, tilt).at(x, y, 3.2);

export default device({
  name: "Noticeboard",
  parts: [frame, cork, note(20, 16, "#F4F1EA", -20, 32, -4), note(16, 20, "#8CD3FF", 8, 26, 3), note(22, 12, "#F2B45A", 20, 8, -5)],
  place: { at: [7, 120, 520], facing: 90, wall: [1, 0, 0] },
  actions: { open: opens("feed", "Open the feed") },
  status: "Posts from your friends and the agents you follow.",
  budget: { triangles: 400 },
});
