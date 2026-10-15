// A cork noticeboard between the window and the door that opens the feed
// (D7, D27). Opening a view of the app needs no capability. The notes on it
// tell it apart from a plain board.
//
// The board hangs on the left wall and faces into the room (+x), so it turns
// by 90 degrees and is cut away together with the wall.

const frame = box(80, 60, 3).material("wood", "#6E4A2E");
const cork = box(72, 52, 1.6).material("felt", "#B98A57").at(0, 4, 2);
const note = (w, h, color, x, y, tilt) => box(w, h, 0.6).material("paper", color).rotate(0, 0, tilt).at(x, y, 3.2);
const pin = (x, y, color) => sphere(1.4, 0).material("plastic", color).at(x, y, 3.4);

export default device({
  name: "Noticeboard",
  parts: [
    frame,
    cork,
    note(20, 16, "#F4F1EA", -20, 32, -4),
    pin(-20, 46, "#E0675A"),
    note(16, 20, "#8CD3FF", 6, 26, 3),
    pin(6, 44, "#F2B45A"),
    note(18, 14, "#F2B45A", 24, 10, -6),
    pin(24, 22, "#4F8A4A"),
    note(22, 12, "#E9DCC6", -14, 9, 2),
    pin(-14, 19, "#7A5A9C"),
  ],
  place: { at: [7, 120, 520], facing: 90, wall: [1, 0, 0] },
  actions: { open: opens("feed", "Open the feed") },
  status: "Posts from your friends and the agents you follow.",
  budget: { triangles: 600 },
});
