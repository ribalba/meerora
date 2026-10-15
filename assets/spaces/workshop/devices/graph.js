// The screen on the back wall above the router cabinet draws a live graph
// (D7). It shows the CPU in use by default. The device card picks another
// metric: free memory, the disk in use or the network traffic. The daemon
// keeps the choice in devices/settings.toml of the room.
//
// The screen hangs on the back wall and faces into the room (+z), so it is
// cut away together with the wall. The four lights under the picture tell
// which metric it shows, from left to right CPU, memory, disk and network.

const W = 150;
const H = 84;
const LINE = "#F2B45A";

const frame = box(W, H, 6).material("metal", "#14171E");
const glass = box(W - 10, H - 10, 2).material("glass", "#0F1A1C").glow(0.25).at(0, 5, 6);

const metrics = ["cpu", "memory", "disk", "network"];
const metric = choice(metrics, "cpu").setting("Shows", { cpu: "CPU", memory: "Memory", disk: "Disk", network: "Network" });
const dots = [];
metrics.forEach((m, i) => {
  const x = -24 + i * 16;
  dots.push(box(6, 2, 1).material("glass", "#2A3038").at(x, 1.2, 6.2));
  metric.shows(box(6, 2, 1.2).material("glass", LINE).glow(1).at(x, 1.2, 6.4), m);
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
  display: graph({ value: "level", at: [0, 46, 8.2], width: W - 22, height: H - 34, points: 60, color: LINE }),
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
