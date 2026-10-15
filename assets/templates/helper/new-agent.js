// A plain felt pebble with two eyes, for an agent that Rivet set up without a
// figure of its own. Give it a look of its own with Edit agent in the builder.

const FELT = "#9AA1B0";
const DARK = "#2A2F3A";

const base = cylinder(13, 2.4, 16).material("stone", DARK);
const body = egg(30, 36, 14).material("felt", FELT).on(base);
const look = eyes(10, 2.6, "#FFE7B0").at(body.front.offset(0, 5, -3.5));
const tuft = cone(3, 7, 6).material("felt", FELT).on(body);

export default figure({
  parts: [base, body, look, tuft],
  anchors: { ring: tuft.top.above(10), face: look },
  animations: {
    working: bob(1.2, 1.1),
    waiting: hop(2, 8).then(face("viewer")),
    resting: breathe(5).eyes("closed"),
  },
  budget: { triangles: 1500 },
});
