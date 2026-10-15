// The hands of the office clock on the back wall show the real time (D7).
// The face is part of the back wall in space.js. The hands hang on the same
// wall and are cut away together with it.
//
// The device sits on the centre of the face, which faces +z. The hour hand
// has 12 positions and the minute hand moves in steps of five minutes.

const INK = "#2A2F3B";

// A tapered hand pointing at `step` of 12, `length` cm long, `z` cm off the face.
function hand(step, length, width, z) {
  const a = (step * 30 * Math.PI) / 180; // clockwise from 12
  const turn = ([x, y]) => [x * Math.cos(a) + y * Math.sin(a), -x * Math.sin(a) + y * Math.cos(a)];
  const outline = [[-width / 2, -3], [width / 2, -3], [width / 5, length], [-width / 5, length]].map(turn);
  return extrude(outline, 1.2).move(0, 0, z).material("metal", INK);
}

const hour = count(0);
const minute = count(0);
for (let i = 0; i < 12; i++) {
  hour.shows(hand(i, 13, 3.2, 1), i);
  minute.shows(hand(i, 20, 2.2, 2.4), i);
}
const pin = cylinder(2, 4, 8).rotate(90, 0, 0).at(0, -2, 1.6).material("brass", "#C8A24A");

export default device({
  name: "Clock",
  parts: [pin],
  params: { hour, minute, time: text("") },
  place: { at: [470, 240, 8], wall: [0, 0, 1] },
  bind: {
    hour: from("clock.time").div(60).mod(12),
    minute: from("clock.time").mod(60).div(5),
    time: from("clock.time").time(),
  },
  status: "It is {time}.",
  budget: { triangles: 500 },
});
