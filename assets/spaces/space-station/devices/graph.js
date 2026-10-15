// The monitor on the back wall above the server rack draws a live graph
// (D7). It shows the CPU in use by default. The device card picks another
// metric: free memory, the disk in use or the network traffic. The daemon
// keeps the choice in devices/settings.toml of the room.
//
// The monitor hangs on the back wall and faces into the module (+z), so it
// is cut away together with the wall. The four lights under the picture
// tell which metric it shows, from left to right CPU, memory, disk and
// network.

const W = 150;
const H = 84;
const LINE = "#7FE3D0";

const frame = box(W, H, 6).material("metal", "#1E232D").bevel(3);
const glass = box(W - 12, H - 12, 2).material("glass", "#081A20").glow(0.3).at(0, 6, 6);

const metrics = ["cpu", "memory", "disk", "network"];
const metric = choice(metrics, "cpu").setting("Shows", { cpu: "CPU", memory: "Memory", disk: "Disk", network: "Network" });
const dots = [];
metrics.forEach((m, i) => {
  const x = -24 + i * 16;
  dots.push(box(6, 2, 1).material("glass", "#2A3038").at(x, 1.5, 6.2));
  metric.shows(box(6, 2, 1.2).material("glass", LINE).glow(1).at(x, 1.5, 6.4), m);
});

export default device({
  name: "System graph",
  parts: [frame, glass, ...dots],
  params: {
    metric,
    level: number(0, 1, 0),
    reading: text(""),
  },
  place: { at: [650, 108, 0], wall: [0, 0, 1] },
  bind: {
    level: per("metric", {
      cpu: "system.cpu",
      memory: "system.memory",
      disk: "system.disk",
      network: from("system.network").log(100e6),
    }),
    reading: per("metric", {
      cpu: from("system.cpu").percent(),
      memory: from("system.memory").percent(),
      disk: from("system.disk").percent(),
      network: from("system.network").bytes(),
    }),
  },
  display: graph({ value: "level", at: [0, 46, 8.2], width: W - 24, height: H - 36, points: 60, color: LINE }),
  status: {
    metric: {
      cpu: "CPU in use: {reading}.",
      memory: "Memory free: {reading}.",
      disk: "Disk in use: {reading}.",
      network: "Network traffic: {reading} per second.",
    },
  },
  budget: { triangles: 400 },
});
