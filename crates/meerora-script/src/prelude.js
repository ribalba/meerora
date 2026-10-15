// meerora modelling API (D4, D5, D20, D24). Evaluated before every script.
// Shapes are built by the host and live in a per-run arena. This file keeps
// track of parts, their transforms and materials, and turns the default export
// into a JSON description that the host bakes into meshes.
"use strict";
(() => {
  const N = globalThis.__meerora_native;
  delete globalThis.__meerora_native;
  const G = globalThis;
  const D2R = Math.PI / 180;

  // ---------------------------------------------------------------- checks

  const show = (v) => {
    if (typeof v === "string") return JSON.stringify(v);
    if (v === null) return "null";
    if (Array.isArray(v)) return "an array";
    if (v instanceof Part) return "a part";
    if (typeof v === "function") return "a function";
    return typeof v === "object" ? "an object" : String(v);
  };
  const fail = (msg) => {
    throw new TypeError(msg);
  };
  const num = (v, what) => (typeof v === "number" && Number.isFinite(v) ? v : fail(`${what} must be a number, got ${show(v)}`));
  const pos = (v, what) => (num(v, what) > 0 ? v : fail(`${what} must be greater than 0, got ${v}`));
  const nonNeg = (v, what) => (num(v, what) >= 0 ? v : fail(`${what} must not be negative, got ${v}`));
  const whole = (v, what) => Math.round(num(v, what));
  const str = (v, what) => (typeof v === "string" ? v : fail(`${what} must be a string, got ${show(v)}`));
  const colour = (v, what) => (typeof v === "string" && N.color(v) ? v : fail(`${what} must be a colour like "#D9A441", got ${show(v)}`));
  const isObj = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
  const options = (v, keys, what) => {
    if (v === undefined || v === null) return {};
    if (!isObj(v)) fail(`${what} must be an object like { ${keys[0]}: ... }, got ${show(v)}`);
    for (const k of Object.keys(v)) {
      if (!keys.includes(k)) fail(`${what} has no option ${JSON.stringify(k)}. Known options: ${keys.join(", ")}`);
    }
    return v;
  };

  // -------------------------------------------------------------- matrices
  // 4x4, column major like glam's Mat4::from_cols_array.

  const ident = () => [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
  const mul = (a, b) => {
    const o = new Array(16);
    for (let c = 0; c < 4; c++) {
      for (let r = 0; r < 4; r++) {
        o[c * 4 + r] = a[r] * b[c * 4] + a[4 + r] * b[c * 4 + 1] + a[8 + r] * b[c * 4 + 2] + a[12 + r] * b[c * 4 + 3];
      }
    }
    return o;
  };
  const translation = (x, y, z) => [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, x, y, z, 1];
  const scaling = (x, y, z) => [x, 0, 0, 0, 0, y, 0, 0, 0, 0, z, 0, 0, 0, 0, 1];
  const rotX = (deg) => {
    const c = Math.cos(deg * D2R), s = Math.sin(deg * D2R);
    return [1, 0, 0, 0, 0, c, s, 0, 0, -s, c, 0, 0, 0, 0, 1];
  };
  const rotY = (deg) => {
    const c = Math.cos(deg * D2R), s = Math.sin(deg * D2R);
    return [c, 0, -s, 0, 0, 1, 0, 0, s, 0, c, 0, 0, 0, 0, 1];
  };
  const rotZ = (deg) => {
    const c = Math.cos(deg * D2R), s = Math.sin(deg * D2R);
    return [c, s, 0, 0, -s, c, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
  };
  const apply = (m, p) => [
    m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
    m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
    m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
  ];
  // M applied around the point c.
  const around = (c, m) => mul(translation(c[0], c[1], c[2]), mul(m, translation(-c[0], -c[1], -c[2])));

  // ---------------------------------------------------------------- points

  const PointProto = Object.create(Array.prototype);
  const point = (x, y, z) => Object.setPrototypeOf([x, y, z], PointProto);
  Object.defineProperties(PointProto, {
    above: {
      value(d = 0) {
        return point(this[0], this[1] + num(d, "point.above(d)"), this[2]);
      },
    },
    offset: {
      value(dx = 0, dy = 0, dz = 0) {
        return point(this[0] + num(dx, "point.offset(dx)"), this[1] + num(dy, "point.offset(dy)"), this[2] + num(dz, "point.offset(dz)"));
      },
    },
  });
  const isPoint = (v) => Array.isArray(v) && v.length === 3 && v.every((c) => typeof c === "number" && Number.isFinite(c));
  const toPoint = (v, what) => (isPoint(v) ? point(v[0], v[1], v[2]) : fail(`${what} must be a point [x, y, z], got ${show(v)}`));

  // ----------------------------------------------------------------- parts

  const FINISHES = "matte, enamel, ceramic, felt, glaze, paper, brass, metal, wood, leather, glass, plastic, stone, foliage";

  class Part {
    constructor(mesh) {
      this._mesh = mesh; // arena id, -1 for groups
      this._m = ident();
      this._mat = { finish: "matte", color: "#FFFFFF", glow: 0 };
      this._kids = [];
      this._parent = null;
      this._box = null; // [w, h, d] for box(), so bevel() can rebuild it
      this._marks = null; // named points in the part's own frame, like a rod's tip
    }

    _each(fn) {
      fn(this);
      for (const k of this._kids) k._each(fn);
    }

    _transform(m) {
      this._each((p) => {
        p._m = mul(m, p._m);
      });
      return this;
    }

    // [minX, minY, minZ, maxX, maxY, maxZ] in world space, children included.
    _box6() {
      const b = [Infinity, Infinity, Infinity, -Infinity, -Infinity, -Infinity];
      this._each((p) => {
        if (p._mesh < 0) return;
        const q = N.bounds(p._mesh, p._m);
        if (!(q[0] <= q[3])) return;
        for (let i = 0; i < 3; i++) {
          b[i] = Math.min(b[i], q[i]);
          b[i + 3] = Math.max(b[i + 3], q[i + 3]);
        }
      });
      if (!(b[0] <= b[3])) {
        const o = apply(this._m, [0, 0, 0]);
        return [o[0], o[1], o[2], o[0], o[1], o[2]];
      }
      return b;
    }

    _mark(name, local) {
      (this._marks ??= {})[name] = local;
      Object.defineProperty(this, name, {
        get() {
          const p = apply(this._m, this._marks[name]);
          return point(p[0], p[1], p[2]);
        },
        configurable: true,
        enumerable: false,
      });
      return this;
    }

    get bounds() {
      const b = this._box6();
      return { min: point(b[0], b[1], b[2]), max: point(b[3], b[4], b[5]) };
    }
    get top() {
      const b = this._box6();
      return point((b[0] + b[3]) / 2, b[4], (b[2] + b[5]) / 2);
    }
    get bottom() {
      const b = this._box6();
      return point((b[0] + b[3]) / 2, b[1], (b[2] + b[5]) / 2);
    }
    get center() {
      const b = this._box6();
      return point((b[0] + b[3]) / 2, (b[1] + b[4]) / 2, (b[2] + b[5]) / 2);
    }
    get front() {
      const b = this._box6();
      return point((b[0] + b[3]) / 2, (b[1] + b[4]) / 2, b[5]);
    }
    get back() {
      const b = this._box6();
      return point((b[0] + b[3]) / 2, (b[1] + b[4]) / 2, b[2]);
    }
    get left() {
      const b = this._box6();
      return point(b[0], (b[1] + b[4]) / 2, (b[2] + b[5]) / 2);
    }
    get right() {
      const b = this._box6();
      return point(b[3], (b[1] + b[4]) / 2, (b[2] + b[5]) / 2);
    }

    // ------------------------------------------------------------ material

    material(finish, color) {
      if (typeof finish !== "string" || !N.finish(finish)) fail(`material() finish must be one of ${FINISHES}, got ${show(finish)}`);
      colour(color, "material() colour");
      const f = finish.toLowerCase();
      return this._paint((m) => {
        m.finish = f;
        m.color = color;
      });
    }
    color(color) {
      colour(color, "color()");
      return this._paint((m) => {
        m.color = color;
      });
    }
    glow(intensity = 1) {
      nonNeg(intensity, "glow(intensity)");
      return this._paint((m) => {
        m.glow = intensity;
      });
    }
    _paint(fn) {
      this._each((p) => fn(p._mat));
      return this;
    }

    // ----------------------------------------------------------- placement

    at(x, y, z) {
      const p = isPoint(x) ? x : [num(x, "at(x)"), num(y, "at(y)"), num(z, "at(z)")];
      const b = this.bottom;
      return this._transform(translation(p[0] - b[0], p[1] - b[1], p[2] - b[2]));
    }
    move(dx = 0, dy = 0, dz = 0) {
      return this._transform(translation(num(dx, "move(dx)"), num(dy, "move(dy)"), num(dz, "move(dz)")));
    }
    rotate(x = 0, y = 0, z = 0) {
      const r = mul(rotZ(num(z, "rotate(z)")), mul(rotY(num(y, "rotate(y)")), rotX(num(x, "rotate(x)"))));
      return this._transform(around(this.bottom, r));
    }
    scale(sx, sy, sz) {
      if (sy === undefined && sz === undefined) sy = sz = sx;
      const f = [sx, sy, sz].map((v, i) => num(v, `scale(${"xyz"[i]})`));
      if (f.some((v) => v === 0)) fail("scale() factors must not be 0");
      return this._transform(around(this.bottom, scaling(f[0], f[1], f[2])));
    }
    on(other, opts) {
      const o = options(opts, ["x", "z"], "on()");
      const t = target(other, "on()", "top");
      return this.at(t[0] + num(o.x ?? 0, "on() x"), t[1], t[2] + num(o.z ?? 0, "on() z"));
    }
    above(other, gap = 0) {
      const t = target(other, "above()", "top");
      return this.at(t[0], t[1] + num(gap, "above(gap)"), t[2]);
    }
    below(other, gap = 0) {
      const t = target(other, "below()", "bottom");
      const top = this.top;
      return this._transform(translation(t[0] - top[0], t[1] - num(gap, "below(gap)") - top[1], t[2] - top[2]));
    }
    beside(other, side, gap = 0) {
      if (!(other instanceof Part)) fail(`beside() needs a part to stand beside, got ${show(other)}`);
      num(gap, "beside(gap)");
      const o = other._box6();
      const b = this._box6();
      const cx = (o[0] + o[3]) / 2 - (b[0] + b[3]) / 2;
      const cz = (o[2] + o[5]) / 2 - (b[2] + b[5]) / 2;
      const dy = o[1] - b[1];
      switch (side) {
        case "left":
          return this._transform(translation(o[0] - gap - b[3], dy, cz));
        case "right":
          return this._transform(translation(o[3] + gap - b[0], dy, cz));
        case "front":
          return this._transform(translation(cx, dy, o[5] + gap - b[2]));
        case "back":
          return this._transform(translation(cx, dy, o[2] - gap - b[5]));
        default:
          return fail(`beside() side must be "left", "right", "front" or "back", got ${show(side)}`);
      }
    }

    // ----------------------------------------------------------- hierarchy

    add(...children) {
      for (const c of flat(children, "add()")) {
        for (let p = this; p; p = p._parent) {
          if (p === c) fail("add() would make a part its own child");
        }
        if (c._parent) c._parent._kids = c._parent._kids.filter((k) => k !== c);
        c._parent = this;
        this._kids.push(c);
      }
      return this;
    }
    clone() {
      const c = new Part(this._mesh);
      if (this._mesh >= 0) N.retain(this._mesh);
      c._m = this._m.slice();
      c._mat = { ...this._mat };
      c._box = this._box;
      if (this._marks) for (const [k, v] of Object.entries(this._marks)) c._mark(k, v);
      for (const k of this._kids) {
        const kc = k.clone();
        kc._parent = c;
        c._kids.push(kc);
      }
      return c;
    }

    bevel(r) {
      if (!this._box) fail("bevel() only works on box() parts");
      this._mesh = N.prim("bevel_box", [...this._box, nonNeg(r, "bevel(r)")], []);
      return this;
    }

    toJSON() {
      const b = this.bounds;
      return { part: { min: [...b.min], max: [...b.max] } };
    }
  }

  const target = (other, what, face) => {
    if (other instanceof Part) return other[face];
    if (isPoint(other)) return other;
    return fail(`${what} needs a part or a point [x, y, z], got ${show(other)}`);
  };

  const flat = (list, what, out = []) => {
    for (const v of list) {
      if (Array.isArray(v)) flat(v, what, out);
      else if (v instanceof Part) out.push(v);
      else fail(`${what} expects parts, got ${show(v)}`);
    }
    return out;
  };

  // ---------------------------------------------------------------- shapes

  const prim = (kind, nums, pts = []) => new Part(N.prim(kind, nums, pts));
  const seg = (v, def, what) => whole(v ?? def, what);
  const pointList = (v, dims, what, max) => {
    if (!Array.isArray(v)) fail(`${what} must be an array of points, got ${show(v)}`);
    if (v.length > max) fail(`${what} has ${v.length} points, at most ${max} are allowed`);
    return v.map((p, i) => {
      if (!Array.isArray(p) || p.length !== dims || !p.every((c) => typeof c === "number" && Number.isFinite(c))) {
        fail(`${what}[${i}] must be [${dims === 2 ? "x, y" : "x, y, z"}], got ${show(p)}`);
      }
      return p.slice();
    });
  };

  G.box = (w, h, d) => {
    const dims = [pos(w, "box(w)"), pos(h, "box(h)"), pos(d, "box(d)")];
    const p = prim("box", dims);
    p._box = dims;
    return p;
  };
  G.cylinder = (r, h, segments) => prim("cylinder", [pos(r, "cylinder(r)"), pos(h, "cylinder(h)"), seg(segments, 12, "cylinder(segments)")]);
  G.cone = (rBottom, h, segments, rTop = 0) =>
    prim("frustum", [nonNeg(rBottom, "cone(rBottom)"), nonNeg(rTop, "cone(rTop)"), pos(h, "cone(h)"), seg(segments, 12, "cone(segments)")]);
  G.sphere = (r, detail) => prim("sphere", [pos(r, "sphere(r)"), seg(detail, 1, "sphere(detail)")]);
  G.capsule = (r, h, segments) => prim("capsule", [pos(r, "capsule(r)"), pos(h, "capsule(h)"), seg(segments, 10, "capsule(segments)")]);
  G.egg = (w, h, segments) => prim("egg", [pos(w, "egg(w)"), pos(h, "egg(h)"), seg(segments, 12, "egg(segments)")]);
  G.lathe = (profile, segments) => {
    const pts = pointList(profile, 2, "lathe(profile)", 1024);
    for (const [i, p] of pts.entries()) nonNeg(p[0], `lathe(profile)[${i}] radius`);
    return prim("lathe", [seg(segments, 12, "lathe(segments)")], pts);
  };
  G.torus = (R, r, segments, sides) =>
    prim("torus", [pos(R, "torus(R)"), pos(r, "torus(r)"), seg(segments, 16, "torus(segments)"), seg(sides, 6, "torus(sides)")]);
  G.tube = (points, r, sides) => prim("tube", [pos(r, "tube(r)"), seg(sides, 6, "tube(sides)")], pointList(points, 3, "tube(points)", 1024));
  G.extrude = (outline, depth) => prim("extrude", [pos(depth, "extrude(depth)")], pointList(outline, 2, "extrude(outline)", 256));
  G.quad = (w, d) => prim("quad", [pos(w, "quad(w)"), pos(d, "quad(d)")]);
  G.group = (...parts) => new Part(-1).add(...parts);

  // --------------------------------------------------------- library parts

  const arc = (cx, cy, r, a0, a1, steps) => {
    const out = [];
    for (let i = 0; i <= steps; i++) {
      const a = (a0 + ((a1 - a0) * i) / steps) * D2R;
      out.push([cx + r * Math.cos(a), cy + r * Math.sin(a)]);
    }
    return out;
  };
  // Disc of radius r and thickness t facing +z, bottom centre at the origin.
  const disc = (r, t, segments = 10) => G.cylinder(r, t, segments).rotate(90, 0, 0).at(0, 0, 0);

  G.bentRod = (length, opts) => {
    const o = options(opts, ["bend", "r", "color"], "bentRod()");
    const len = pos(length, "bentRod(length)");
    const bend = num(o.bend ?? 30, "bentRod() bend") * D2R;
    const r = pos(o.r ?? 1.2, "bentRod() r");
    const h = len / 2;
    const pts = [
      [0, 0, 0],
      [0, h, 0],
      [h * Math.sin(bend), h + h * Math.cos(bend), 0],
    ];
    const rod = G.tube(pts, r, 6).material("metal", colour(o.color ?? "#8A8F9A", "bentRod() color"));
    return rod._mark("tip", pts[2]);
  };

  G.bulb = (color, r = 4) => G.sphere(pos(r, "bulb(r)"), 1).material("glass", colour(color, "bulb(color)")).glow(1);

  G.screen = (w, h, color = "#1B2A2A") =>
    G.box(pos(w, "screen(w)"), pos(h, "screen(h)"), 1).material("glass", colour(color, "screen(color)")).glow(1);

  G.eyes = (gap, r = 3, color = "#FFE7B0") => {
    const g = pos(gap, "eyes(gap)");
    const rr = pos(r, "eyes(r)");
    colour(color, "eyes(color)");
    const eye = (x) => disc(rr, 1, 10).at(x, 0, 0).material("glass", color).glow(1);
    return G.group(eye(-g / 2), eye(g / 2));
  };

  G.treads = (opts) => {
    const o = options(opts, ["wheels", "width"], "treads()");
    const n = Math.max(1, whole(o.wheels ?? 3, "treads() wheels"));
    const w = pos(o.width ?? 50, "treads() width");
    const h = w * 0.34;
    const d = w * 0.6;
    const r = h / 2;
    const outline = [...arc(w / 2 - r, r, r, -90, 90, 6), ...arc(-w / 2 + r, r, r, 90, 270, 6)];
    const belt = G.extrude(outline, d).material("plastic", "#3B404C");
    const parts = [belt];
    for (let i = 0; i < n; i++) {
      const x = n === 1 ? 0 : -w / 2 + r + ((w - 2 * r) * i) / (n - 1);
      const wheel = disc(r * 0.62, 1.2, 10).material("metal", "#6B7282");
      parts.push(wheel.at(x, r - r * 0.62, d / 2));
      parts.push(disc(r * 0.24, 0.8, 8).material("metal", "#3B404C").at(x, r - r * 0.24, d / 2 + 0.9));
      parts.push(wheel.clone().move(0, 0, -d));
    }
    return G.group(...parts);
  };

  G.wrench = (size = 12) => {
    const s = pos(size, "wrench(size)");
    const hw = s * 0.09; // half handle width
    const R = s * 0.26; // jaw radius
    const nw = s * 0.09; // half notch width
    const yc = s - R; // jaw centre
    const neck = Math.acos(hw / R) / D2R;
    const lip = Math.acos(nw / R) / D2R;
    const outline = [
      [-hw, 0],
      [hw, 0],
      ...arc(0, yc, R, -neck, lip, 4),
      [nw, yc + R * 0.1],
      [-nw, yc + R * 0.1],
      ...arc(0, yc, R, 180 - lip, 180 + neck, 4),
    ];
    return G.extrude(outline, s * 0.12).at(0, 0, 0).material("metal", "#8A8F9A");
  };

  G.vents = (count) => {
    const n = Math.max(1, whole(count, "vents(count)"));
    const slats = [];
    for (let i = 0; i < n; i++) slats.push(G.box(16, 2.4, 1).material("enamel", "#9A927F").at(0, i * 5, 0));
    return G.group(...slats);
  };

  G.dial = () => {
    const face = disc(7, 1.2, 14).material("ceramic", "#F7F3EA");
    const rim = G.torus(7, 0.6, 14, 4).rotate(90, 0, 0).at(0, -0.6, 0.6).material("metal", "#9A927F");
    const needle = G.box(0.9, 5.5, 0.6).material("enamel", "#C8553D").at(0, 7, 0.9).rotate(0, 0, -40);
    return G.group(face, rim, needle);
  };

  const EMBLEMS = {
    lock: (s, c) => {
      const body = G.box(s, s * 0.8, s * 0.15).bevel(s * 0.05).material("enamel", c ?? "#7C6FD0");
      const shackle = G.torus(s * 0.3, s * 0.07, 12, 4).rotate(90, 0, 0).material("metal", c ?? "#7C6FD0");
      shackle.at(0, s * 0.8 - s * 0.37, 0);
      return G.group(body, shackle);
    },
    wifi: (s, c) => {
      const band = (r0, r1) => G.extrude([...arc(0, 0, r1, 45, 135, 6), ...arc(0, 0, r0, 135, 45, 6)], s * 0.08);
      const g = G.group(band(s * 0.62, s * 0.78), band(s * 0.32, s * 0.48), G.cylinder(s * 0.09, s * 0.08, 8).rotate(90, 0, 0));
      g._kids[2].at(0, 0, 0);
      return g.material("glass", c ?? "#7FE3D0").glow(1).at(0, 0, 0);
    },
    bolt: (s, c) =>
      G.extrude(
        [
          [0.1, 1],
          [-0.35, 0.05],
          [-0.02, 0.05],
          [-0.15, -0.5],
          [0.35, 0.4],
          [0.02, 0.4],
        ].map(([x, y]) => [x * s, y * s]),
        s * 0.12,
      )
        .at(0, 0, 0)
        .material("enamel", c ?? "#F2B45A"),
    star: (s, c) => {
      const pts = [];
      for (let i = 0; i < 10; i++) {
        const a = (90 + i * 36) * D2R;
        const r = i % 2 === 0 ? s / 2 : s / 5;
        pts.push([r * Math.cos(a), r * Math.sin(a)]);
      }
      return G.extrude(pts, s * 0.12).at(0, 0, 0).material("enamel", c ?? "#F2B45A");
    },
    heart: (s, c) => {
      const pts = [];
      for (let i = 0; i < 16; i++) {
        const t = (i / 16) * Math.PI * 2;
        pts.push([16 * Math.sin(t) ** 3, 13 * Math.cos(t) - 5 * Math.cos(2 * t) - 2 * Math.cos(3 * t) - Math.cos(4 * t)].map((v) => (v * s) / 34));
      }
      return G.extrude(pts, s * 0.12).at(0, 0, 0).material("enamel", c ?? "#E0675A");
    },
    mail: (s, c) => {
      const env = G.box(s, s * 0.66, s * 0.1).material("paper", c ?? "#F4F1EA");
      const flap = G.extrude(
        [
          [-s * 0.46, s * 0.62],
          [0, s * 0.28],
          [s * 0.46, s * 0.62],
        ],
        s * 0.04,
      )
        .material("paper", "#D8D2C4")
        .move(0, 0, s * 0.07);
      return G.group(env, flap);
    },
  };

  G.emblem = (name, opts) => {
    const o = options(opts, ["size", "color"], "emblem()");
    if (typeof name !== "string" || !Object.hasOwn(EMBLEMS, name)) fail(`emblem() knows ${Object.keys(EMBLEMS).join(", ")}, got ${show(name)}`);
    const c = o.color === undefined ? undefined : colour(o.color, "emblem() color");
    return EMBLEMS[name](pos(o.size ?? 10, "emblem() size"), c);
  };

  // ------------------------------------------------------------ animations

  class Anim {
    constructor(motion) {
      this._motion = motion;
      this._face = false;
      this._closed = false;
    }
    _with(fn) {
      const a = new Anim(this._motion);
      a._face = this._face;
      a._closed = this._closed;
      fn(a);
      return a;
    }
    then(next) {
      if (!(next instanceof Facing)) fail('then() takes face("viewer")');
      return this._with((a) => {
        a._face = true;
      });
    }
    eyes(state) {
      if (state !== "closed" && state !== "open") fail(`eyes() takes "closed" or "open", got ${show(state)}`);
      return this._with((a) => {
        a._closed = state === "closed";
      });
    }
  }
  class Facing {}

  G.bob = (amplitude = 2, period = 1.4) => new Anim({ type: "bob", amplitude: num(amplitude, "bob(amplitude)"), period: pos(period, "bob(period)") });
  G.hop = (count = 1, height = 12, period = 2.4) =>
    new Anim({ type: "hop", count: Math.min(1000, Math.max(1, whole(count, "hop(count)"))), height: num(height, "hop(height)"), period: pos(period, "hop(period)") });
  G.breathe = (period = 4, depth = 0.02) => new Anim({ type: "breathe", period: pos(period, "breathe(period)"), depth: num(depth, "breathe(depth)") });
  G.spin = (period = 4) => new Anim({ type: "spin", period: num(period, "spin(period)") || fail("spin(period) must not be 0") });
  G.wave = (period = 1.5, angle = 8) => new Anim({ type: "wave", period: pos(period, "wave(period)"), angle: num(angle, "wave(angle)") });
  G.orbit = (radius = 20, period = 4) => new Anim({ type: "orbit", radius: num(radius, "orbit(radius)"), period: num(period, "orbit(period)") || fail("orbit(period) must not be 0") });
  G.still = () => new Anim({ type: "still" });
  G.face = (what) => (what === "viewer" ? new Facing() : fail(`face() takes "viewer", got ${show(what)}`));

  // ---------------------------------------------------------------- params

  class Param {
    constructor(kind, check) {
      this._kind = kind;
      this._check = check;
      this._shows = [];
    }
    shows(part, when = true, opts) {
      if (!(part instanceof Part)) fail(`shows() needs a part, got ${show(part)}`);
      this._check(when, "shows(when)");
      const o = options(opts, ["pivot"], "shows()");
      const pivot = o.pivot === undefined ? null : toPoint(o.pivot, "shows() pivot");
      this._shows.push({ part, when, pivot, anim: null });
      return this;
    }
    animate(motion) {
      const last = this._shows[this._shows.length - 1];
      if (!last) fail("animate() needs a shows(part) before it");
      if (!(motion instanceof Anim)) fail(`animate() takes a motion such as wave(), got ${show(motion)}`);
      last.anim = motion._motion;
      return this;
    }
    // A device param the user sets on the device card. The daemon keeps the
    // choice in the room's devices/settings.toml. Only devices use it.
    setting(text, labels) {
      if (this._kind.type !== "choice" && this._kind.type !== "toggle") fail("setting() works on choice() and toggle() params");
      const name = str(text, "setting(label)");
      if (name.length === 0 || name.length > 40) fail("setting(label) must be 1 to 40 characters long");
      const names = {};
      if (labels !== undefined && labels !== null) {
        if (!isObj(labels)) fail("setting(label, labels) takes labels like { cpu: \"CPU\" }");
        for (const [k, v] of Object.entries(labels)) {
          this._check(this._kind.type === "toggle" ? k === "true" : k, `setting() labels.${k}`);
          const l = str(v, `setting() labels.${k}`);
          if (l.length === 0 || l.length > 24) fail(`setting() labels.${k} must be 1 to 24 characters long`);
          names[k] = l;
        }
      }
      this._setting = { label: name, labels: names };
      return this;
    }
  }

  const isBool = (v, what) => typeof v === "boolean" || fail(`${what} must be true or false, got ${show(v)}`);
  G.toggle = (def = false) => {
    isBool(def, "toggle(default)");
    return new Param({ type: "toggle", default: def }, isBool);
  };
  G.choice = (values, def) => {
    if (!Array.isArray(values) || values.length === 0 || !values.every((v) => typeof v === "string")) fail("choice(values) must be a non-empty array of strings");
    const check = (v, what) => values.includes(v) || fail(`${what} must be one of ${values.map((x) => JSON.stringify(x)).join(", ")}, got ${show(v)}`);
    def ??= values[0];
    check(def, "choice(default)");
    return new Param({ type: "choice", values: values.slice(), default: def }, check);
  };
  G.number = (min, max, def) => {
    num(min, "number(min)");
    num(max, "number(max)");
    if (min > max) fail("number(min, max) needs min <= max");
    def ??= min;
    const check = (v, what) => (num(v, what) >= min && v <= max) || fail(`${what} must be between ${min} and ${max}, got ${v}`);
    check(def, "number(default)");
    return new Param({ type: "number", min, max, default: def }, check);
  };
  G.count = (def = 0) => {
    const check = (v, what) => (Number.isInteger(v) && v >= 0 && v <= 4294967295) || fail(`${what} must be a whole number of at least 0, got ${show(v)}`);
    check(def, "count(default)");
    return new Param({ type: "count", default: def }, check);
  };
  G.text = (def = "") => {
    str(def, "text(default)");
    if (def.length > 200) fail("text(default) is limited to 200 characters");
    return new Param({ type: "text", default: def }, (v, what) => str(v, what));
  };

  // ----------------------------------------------------------- the export

  const entry = (p) => ({ mesh: p._mesh, m: p._m, color: p._mat.color, finish: p._mat.finish, glow: p._mat.glow });

  // Mesh parts reachable from `roots`, skipping hidden subtrees and repeats.
  const collect = (roots, hidden, what) => {
    const out = [];
    const seen = new Set();
    const visit = (p) => {
      if (hidden.has(p) || seen.has(p)) return;
      seen.add(p);
      if (p._mesh >= 0) out.push(entry(p));
      for (const k of p._kids) visit(k);
    };
    for (const p of flat(roots, what)) visit(p);
    return out;
  };

  const known = (obj, keys, what) => {
    for (const k of Object.keys(obj)) {
      if (!keys.includes(k)) fail(`${what} has no key ${JSON.stringify(k)}. Known keys: ${keys.join(", ")}`);
    }
  };
  const section = (v, keys, what) => {
    if (v === undefined || v === null) return {};
    if (!isObj(v)) fail(`${what} must be an object, got ${show(v)}`);
    known(v, keys, what);
    return v;
  };
  const budgetOf = (b) => {
    if (b === undefined || b === null) return null;
    const s = section(b, ["triangles"], "budget");
    return s.triangles === undefined ? null : whole(s.triangles, "budget.triangles");
  };
  const built = (kind, desc) => Object.freeze({ __meerora: N.built(kind, JSON.stringify(desc)), kind });

  G.figure = (spec) => {
    if (!isObj(spec)) fail("figure() expects an object like figure({ parts: [...], anchors: {...}, animations: {...} })");
    known(spec, ["parts", "anchors", "animations", "params", "budget"], "figure()");
    const anchors = section(spec.anchors, ["ring", "face"], "anchors");
    const anims = section(spec.animations, ["working", "waiting", "resting"], "animations");
    const params = section(spec.params, Object.keys(spec.params ?? {}), "params");

    const defs = Object.entries(params).map(([name, p]) => {
      if (!(p instanceof Param)) fail(`params.${name} must be toggle(), choice(), number(), count() or text(), got ${show(p)}`);
      return [name, p];
    });
    const hidden = new Set(defs.flatMap(([, p]) => p._shows.map((s) => s.part)));

    const anchor = (v, slot) => {
      if (v === undefined || v === null) return null;
      if (v instanceof Part) return slot === "ring" ? v.top : v.center;
      return toPoint(v, `anchors.${slot}`);
    };
    const anim = (v, slot) => {
      if (v === undefined || v === null) return null;
      if (!(v instanceof Anim)) fail(`animations.${slot} must be a motion such as bob(), got ${show(v)}`);
      return { motion: v._motion, faceViewer: v._face, eyesClosed: v._closed };
    };

    return built("figure", {
      base: collect(spec.parts ?? [], hidden, "figure() parts"),
      anchors: { ring: anchor(anchors.ring, "ring"), face: anchor(anchors.face, "face") },
      animations: { working: anim(anims.working, "working"), waiting: anim(anims.waiting, "waiting"), resting: anim(anims.resting, "resting") },
      params: defs.map(([name, p]) => ({
        name,
        kind: p._kind,
        shows: p._shows.map((s) => {
          const others = new Set([...hidden].filter((h) => h !== s.part));
          return { when: s.when, pivot: s.pivot ?? s.part.bottom, anim: s.anim, parts: collect([s.part], others, "shows()") };
        }),
      })),
      budget: budgetOf(spec.budget),
    });
  };

  class Light {
    constructor(position, color, intensity, radius) {
      this.position = toPoint(position, "light(position)");
      this.color = colour(color, "light(color)");
      this.intensity = nonNeg(intensity, "light(intensity)");
      this.radius = pos(radius, "light(radius)");
    }
  }
  const TOOL = /^[a-z][a-z0-9_]{0,31}$/;
  const toolList = (v) => {
    if (!Array.isArray(v)) fail(`station() tools must be an array of categories like ["terminal", "editor"], got ${show(v)}`);
    const seen = new Set();
    for (const t of v) {
      if (typeof t === "string" && t.includes(".") && TOOL.test(t.split(".")[0])) {
        fail(`station() tools are categories like ${JSON.stringify(t.split(".")[0])}, not action names like ${JSON.stringify(t)}`);
      }
      if (typeof t !== "string" || !TOOL.test(t)) fail(`station() tools entries must be short lowercase names like "terminal", got ${show(t)}`);
      if (seen.has(t)) fail(`station() tools lists ${JSON.stringify(t)} twice`);
      seen.add(t);
    }
    return v.slice();
  };
  class Station {
    constructor(position, facing, front, tools) {
      this.position = toPoint(position, "station(position)");
      this.facing = num(facing, "station() facing");
      this.front = typeof front === "boolean" ? front : fail(`station() front must be true or false, got ${show(front)}`);
      this.tools = toolList(tools);
    }
  }
  G.light = (position, color = "#FFE2B0", intensity = 1, radius = 300) => new Light(position, color, intensity, radius);
  G.station = (position, opts) => {
    const o = options(opts, ["facing", "front", "tools"], "station()");
    return new Station(position, o.facing ?? 0, o.front ?? false, o.tools ?? []);
  };

  G.space = (spec) => {
    if (!isObj(spec)) fail("space() expects an object like space({ parts: [...], stations: {...} })");
    known(spec, ["parts", "walls", "lights", "ambient", "sun", "background", "stations", "waypoints", "paths", "camera", "graveyard", "budget"], "space()");
    const walls = spec.walls ?? [];
    if (!Array.isArray(walls)) fail("walls must be an array like [{ inside: [1, 0, 0], parts: [...] }]");
    const wallRoots = walls.map((w, i) => {
      if (!isObj(w)) fail(`walls[${i}] must be an object like { inside: [1, 0, 0], parts: [...] }, got ${show(w)}`);
      known(w, ["inside", "parts"], `walls[${i}]`);
      return flat([w.parts ?? []], `walls[${i}].parts`);
    });
    const ambient = section(spec.ambient, ["sky", "ground"], "ambient");
    const sun = section(spec.sun, ["direction", "color", "intensity"], "sun");
    const camera = section(spec.camera, ["target", "viewHeight", "azimuth", "elevation"], "camera");
    const lights = spec.lights ?? [];
    if (!Array.isArray(lights)) fail("lights must be an array of light(...)");
    const stations = section(spec.stations, Object.keys(spec.stations ?? {}), "stations");
    const waypoints = section(spec.waypoints, Object.keys(spec.waypoints ?? {}), "waypoints");
    const paths = spec.paths ?? [];
    if (!Array.isArray(paths)) fail('paths must be an array of pairs like ["desk", "hall"]');
    const opt = (v, f, what) => (v === undefined || v === null ? null : f(v, what));
    // The plot where the app stands one tombstone per retired agent, in front
    // of a station in the direction it faces.
    let graveyard = null;
    if (spec.graveyard !== undefined && spec.graveyard !== null) {
      const g = section(spec.graveyard, ["station", "gap", "width", "depth"], "graveyard");
      graveyard = {
        station: str(g.station ?? "graveyard", "graveyard.station"),
        gap: nonNeg(g.gap ?? 0, "graveyard.gap"),
        width: pos(g.width, "graveyard.width"),
        depth: pos(g.depth, "graveyard.depth"),
      };
    }

    return built("space", {
      base: collect(spec.parts ?? [], new Set(wallRoots.flat()), "space() parts"),
      walls: walls.map((w, i) => {
        const inside = toPoint(w.inside, `walls[${i}].inside`);
        if (Math.hypot(inside[0], inside[2]) < 1e-6) fail(`walls[${i}].inside must point along the floor into the room, like [1, 0, 0], got ${show(w.inside)}`);
        const others = new Set(wallRoots.filter((_, j) => j !== i).flat());
        return { inside: [inside[0], inside[1], inside[2]], parts: collect(wallRoots[i], others, `walls[${i}].parts`) };
      }),
      lights: lights.map((l, i) => {
        if (!(l instanceof Light)) fail(`lights[${i}] must be light(position, color, intensity, radius)`);
        return { position: l.position, color: l.color, intensity: l.intensity, radius: l.radius };
      }),
      ambient: { sky: colour(ambient.sky ?? "#2A3550", "ambient.sky"), ground: colour(ambient.ground ?? "#1A1410", "ambient.ground") },
      sun: {
        direction: toPoint(sun.direction ?? [-0.4, -1, -0.3], "sun.direction"),
        color: colour(sun.color ?? "#FFF1DC", "sun.color"),
        intensity: nonNeg(sun.intensity ?? 0.6, "sun.intensity"),
      },
      background: colour(spec.background ?? "#0B0D12", "background"),
      stations: Object.entries(stations).map(([id, s]) => {
        const st = s instanceof Station ? s : isPoint(s) ? new Station(s, 0, false, []) : fail(`stations.${id} must be station([x, y, z], {...}), got ${show(s)}`);
        return { id, position: st.position, facing: st.facing, front: st.front, tools: st.tools };
      }),
      waypoints: Object.entries(waypoints).map(([id, p]) => ({ id, position: toPoint(p, `waypoints.${id}`) })),
      paths: paths.map((p, i) => {
        if (!Array.isArray(p) || p.length !== 2 || typeof p[0] !== "string" || typeof p[1] !== "string") fail(`paths[${i}] must be a pair of ids like ["desk", "hall"], got ${show(p)}`);
        return [p[0], p[1]];
      }),
      camera: {
        target: opt(camera.target, toPoint, "camera.target"),
        viewHeight: opt(camera.viewHeight, pos, "camera.viewHeight"),
        azimuth: opt(camera.azimuth, num, "camera.azimuth"),
        elevation: opt(camera.elevation, num, "camera.elevation"),
      },
      graveyard,
      budget: budgetOf(spec.budget),
    });
  };

  // ------------------------------------------------------- devices (D7, D8)
  // A device is shaped like a figure, with parts and params, plus where it
  // stands, which state source drives which param and what a click does.
  // The host checks the source names and the binding types.

  const SCALAR = (v) => typeof v === "string" || typeof v === "boolean" || (typeof v === "number" && Number.isFinite(v));
  class Binding {
    constructor(source, maps) {
      this._source = source;
      this._maps = maps;
    }
    _then(map) {
      return new Binding(this._source, [...this._maps, map]);
    }
    div(n) {
      return this._then({ op: "div", n: pos(n, "div(n)") });
    }
    mod(n) {
      return this._then({ op: "mod", n: pos(n, "mod(n)") });
    }
    steps(bounds, values) {
      if (!Array.isArray(bounds) || !bounds.every((b) => typeof b === "number" && Number.isFinite(b))) fail("steps(bounds) must be an array of numbers");
      for (let i = 1; i < bounds.length; i++) if (!(bounds[i] > bounds[i - 1])) fail("steps(bounds) must grow from left to right");
      if (!Array.isArray(values) || values.length !== bounds.length + 1) fail(`steps(values) needs one value more than bounds, ${bounds.length + 1}, got ${show(values)}`);
      if (!values.every(SCALAR)) fail("steps(values) must be strings, numbers or true and false");
      return this._then({ op: "steps", bounds: bounds.slice(), values: values.slice() });
    }
    is(value) {
      if (!SCALAR(value)) fail(`is(value) takes a string, number or true and false, got ${show(value)}`);
      return this._then({ op: "is", value });
    }
    percent() {
      return this._then({ op: "percent" });
    }
    time() {
      return this._then({ op: "time" });
    }
    log(max) {
      return this._then({ op: "log", max: pos(max, "log(max)") });
    }
    bytes() {
      return this._then({ op: "bytes" });
    }
  }
  G.from = (source) => new Binding(str(source, "from(source)"), []);

  // A binding chosen by the value of a setting param, one per value.
  class PerSetting {
    constructor(setting, cases) {
      this._setting = setting;
      this._cases = cases;
    }
  }
  G.per = (setting, cases) => {
    str(setting, "per(setting)");
    if (!isObj(cases) || Object.keys(cases).length === 0) fail('per(setting, cases) takes cases like { cpu: from("system.cpu") }');
    const list = Object.entries(cases).map(([value, b]) => {
      const binding = typeof b === "string" ? new Binding(b, []) : b;
      if (!(binding instanceof Binding)) fail(`per() cases.${value} must be a source like "system.cpu" or from(...), got ${show(b)}`);
      return [value, binding];
    });
    return new PerSetting(setting, list);
  };

  // A live line graph that the app draws from the recent values of a number
  // param, inside a rectangle of the device facing +z.
  class Graph {
    constructor(spec) {
      this._spec = spec;
    }
  }
  G.graph = (opts) => {
    const o = options(opts, ["value", "at", "width", "height", "points", "min", "max", "color", "thickness"], "graph()");
    const min = num(o.min ?? 0, "graph() min");
    const max = num(o.max ?? 1, "graph() max");
    if (!(max > min)) fail("graph() needs max greater than min");
    const points = whole(o.points ?? 60, "graph() points");
    if (points < 2 || points > 240) fail("graph() points must be between 2 and 240");
    return new Graph({
      value: str(o.value, "graph() value"),
      at: toPoint(o.at ?? [0, 0, 0], "graph() at"),
      width: pos(o.width, "graph() width"),
      height: pos(o.height, "graph() height"),
      points,
      min,
      max,
      color: colour(o.color ?? "#F2B45A", "graph() color"),
      thickness: pos(o.thickness ?? 1.6, "graph() thickness"),
    });
  };

  class Action {
    constructor(spec) {
      this._spec = spec;
    }
  }
  const DOES = { toggle: "Toggle", on: "Switch on", off: "Switch off" };
  const label = (v, def, what) => {
    const l = str(v ?? def, what);
    if (l.length === 0 || l.length > 40) fail(`${what} must be 1 to 40 characters long`);
    return l;
  };
  G.action = (capability, does = "toggle", text) => {
    str(capability, "action(capability)");
    if (!Object.hasOwn(DOES, does)) fail(`action(does) is "toggle", "on" or "off", got ${show(does)}`);
    return new Action({ capability, does, label: label(text, DOES[does], "action(label)") });
  };
  const VIEWS = { feed: "Open the feed" };
  G.opens = (view, text) => {
    if (typeof view !== "string" || !Object.hasOwn(VIEWS, view)) fail(`opens(view) knows ${Object.keys(VIEWS).join(", ")}, got ${show(view)}`);
    return new Action({ opens: view, label: label(text, VIEWS[view], "opens(label)") });
  };

  const ACTION_ID = /^[a-z][a-z0-9_]{0,31}$/;
  G.device = (spec) => {
    if (!isObj(spec)) fail('device() expects an object like device({ name: "Lamp", parts: [...], place: {...} })');
    known(spec, ["name", "parts", "params", "place", "bind", "actions", "status", "display", "budget"], "device()");
    const name = label(spec.name, undefined, "device() name");
    const params = section(spec.params, Object.keys(spec.params ?? {}), "params");
    const defs = Object.entries(params).map(([n, p]) => {
      if (!(p instanceof Param)) fail(`params.${n} must be toggle(), choice(), number(), count() or text(), got ${show(p)}`);
      return [n, p];
    });
    const hidden = new Set(defs.flatMap(([, p]) => p._shows.map((s) => s.part)));

    if (spec.place === undefined || spec.place === null) fail("device() needs a place like { at: [x, y, z] } or { station: \"window\" }");
    const place = section(spec.place, ["at", "station", "offset", "facing", "wall"], "place");
    if ((place.at === undefined) === (place.station === undefined)) fail("place needs either at: [x, y, z] or station: \"id\", not both");
    if (place.at !== undefined && place.offset !== undefined) fail("place.offset only goes with place.station");
    const opt = (v, f, what) => (v === undefined || v === null ? null : f(v, what));
    const wall = opt(place.wall, toPoint, "place.wall");
    if (wall && Math.hypot(wall[0], wall[2]) < 1e-6) fail(`place.wall is the inside direction of the wall along the floor, like [0, 0, 1], got ${show(place.wall)}`);

    const bind = section(spec.bind, Object.keys(spec.bind ?? {}), "bind");
    const bindings = Object.entries(bind).flatMap(([param, b]) => {
      if (!Object.hasOwn(params, param)) fail(`bind.${param} names no param. Declare it in params first`);
      if (b instanceof PerSetting) {
        return b._cases.map(([value, binding]) => ({ param, source: binding._source, maps: binding._maps, when: { setting: b._setting, value } }));
      }
      const binding = typeof b === "string" ? new Binding(b, []) : b;
      if (!(binding instanceof Binding)) fail(`bind.${param} must be a source like "network.airplane_mode", from(...) or per(...), got ${show(b)}`);
      return [{ param, source: binding._source, maps: binding._maps, when: null }];
    });
    const settings = defs.filter(([, p]) => p._setting).map(([n, p]) => ({ param: n, label: p._setting.label, labels: p._setting.labels }));
    let display = null;
    if (spec.display !== undefined && spec.display !== null) {
      if (!(spec.display instanceof Graph)) fail(`display must be graph({...}), got ${show(spec.display)}`);
      display = spec.display._spec;
    }

    const actions = section(spec.actions, Object.keys(spec.actions ?? {}), "actions");
    if (Object.keys(actions).length > 4) fail("a device has at most 4 actions");
    const actionList = Object.entries(actions).map(([id, a]) => {
      if (!ACTION_ID.test(id)) fail(`actions.${id}: action names are short lowercase words like "toggle"`);
      if (!(a instanceof Action)) fail(`actions.${id} must be action(...) or opens(...), got ${show(a)}`);
      return { id, ...a._spec };
    });

    let status = null;
    if (typeof spec.status === "string") status = { template: spec.status };
    else if (spec.status !== undefined && spec.status !== null) {
      if (!isObj(spec.status) || Object.keys(spec.status).length !== 1) fail('status is a text like "{used} used" or { param: { value: "text" } } for one param');
      const [param, texts] = Object.entries(spec.status)[0];
      if (!isObj(texts) || !Object.values(texts).every((t) => typeof t === "string")) fail(`status.${param} maps values of the param to texts`);
      status = { param, texts: { ...texts } };
    }

    return built("device", {
      name,
      base: collect(spec.parts ?? [], hidden, "device() parts"),
      params: defs.map(([n, p]) => ({
        name: n,
        kind: p._kind,
        shows: p._shows.map((s) => {
          const others = new Set([...hidden].filter((h) => h !== s.part));
          return { when: s.when, pivot: s.pivot ?? s.part.bottom, anim: s.anim, parts: collect([s.part], others, "shows()") };
        }),
      })),
      place: {
        at: opt(place.at, toPoint, "place.at"),
        station: opt(place.station, str, "place.station"),
        offset: opt(place.offset, toPoint, "place.offset"),
        facing: opt(place.facing, num, "place.facing"),
        wall,
      },
      bind: bindings,
      settings,
      display,
      actions: actionList,
      status,
      budget: budgetOf(spec.budget),
    });
  };

  // ------------------------------------------------------------- the rest

  const fmt = (v) => {
    if (typeof v === "string") return v;
    try {
      const s = JSON.stringify(v);
      return s === undefined ? String(v) : s;
    } catch {
      return String(v);
    }
  };
  const log = (...args) => N.log(args.map(fmt).join(" "));
  G.console = Object.freeze({ log, info: log, warn: log, error: log, debug: log });

  // Seeded so the same script always builds the same figure (mulberry32).
  let seed = 0x6d656572;
  Math.random = function random() {
    seed = (seed + 0x6d2b79f5) >>> 0;
    let t = seed;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
})();
