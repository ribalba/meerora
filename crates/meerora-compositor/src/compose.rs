//! Composes a window from the shared memory buffers of its toplevel, its
//! subsurfaces and its popups into one RGBA image, in software. There is no
//! dmabuf, so every buffer is `wl_shm` memory.
//!
//! The image keeps the density of the window's own buffer: 1.25 pixels per
//! logical unit for a client that follows `wp_fractional_scale_v1` at 1.25,
//! 2 for a client that renders at buffer scale 2, and 1 for X11 windows.
//! The app scales the texture, so nothing is resampled twice.
//!
//! Only the regions the clients damaged since the last composition are
//! drawn again (damage tracking). The image is drawn whole when its size
//! changed or a surface appeared, went or moved.

use smithay::backend::renderer::utils::{CommitCounter, RendererSurfaceStateUserData, SurfaceView};
use smithay::desktop::{PopupManager, Window};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{SurfaceData, TraversalAction, with_surface_tree_downward};
use smithay::wayland::seat::WaylandFocus as _;
use smithay::wayland::shm::{BufferData, with_buffer_contents};

/// A region of an image in pixels: x, y, width, height.
pub type Rect = [u32; 4];

/// More damaged regions than this are drawn as one bounding box.
const MAX_RECTS: usize = 8;

/// A composed image in premultiplied RGBA, `width * height * 4` bytes at
/// `scale` pixels per logical unit.
pub(crate) struct Canvas {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub pixels: Vec<u8>,
    /// What the last composition drew, in order: each surface, its place in
    /// pixels and its commit.
    drawn: Vec<(WlSurface, Area, CommitCounter)>,
}

impl Default for Canvas {
    fn default() -> Self {
        Self { width: 0, height: 0, scale: 1.0, pixels: Vec::new(), drawn: Vec::new() }
    }
}

/// A rectangle in canvas pixels, from `x0, y0` to `x1, y1` exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Area {
    fn intersect(self, o: Area) -> Option<Area> {
        let a = Area { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) };
        (a.x0 < a.x1 && a.y0 < a.y1).then_some(a)
    }

    fn union(self, o: Area) -> Area {
        Area { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }

    fn rect(self) -> Rect {
        [self.x0 as u32, self.y0 as u32, (self.x1 - self.x0) as u32, (self.y1 - self.y0) as u32]
    }
}

/// One buffer to draw.
struct Layer {
    surface: WlSurface,
    buffer: WlBuffer,
    buffer_scale: i32,
    view: SurfaceView,
    area: Area,
    commit: CommitCounter,
    /// What changed since the last composition, in buffer pixels. `None` is everything.
    damage: Option<Vec<smithay::utils::Rectangle<i32, smithay::utils::Buffer>>>,
}

/// Draws `window` with its popups, and `extra` surfaces on top at their
/// offsets from the window's top left (X11 menus). The image covers the
/// window geometry, so client side shadows outside it are cut away.
///
/// Returns the regions that changed, `None` when the whole image is new.
/// An empty list means that nothing visible changed.
pub(crate) fn compose(window: &Window, extra: &[(WlSurface, Point<i32, Logical>)], canvas: &mut Canvas) -> Option<Vec<Rect>> {
    let geo = window.geometry();
    let Some(root) = window.wl_surface().map(|s| s.into_owned()) else {
        resize(canvas, 1, 1, 1.0);
        return None;
    };
    let scale = density(&root);
    let width = ((f64::from(geo.size.w.max(1)) * scale).round() as u32).max(1);
    let height = ((f64::from(geo.size.h.max(1)) * scale).round() as u32).max(1);

    let mut layers = Vec::new();
    // The image starts at the window geometry, the root surface may start before it.
    collect(&root, Point::from((-geo.loc.x, -geo.loc.y)), scale, &canvas.drawn, &mut layers);
    // Popup positions are relative to the window geometry. Parents come
    // after their children in this list, so it is walked backwards.
    let mut popups: Vec<_> = PopupManager::popups_for_surface(&root).collect();
    popups.reverse();
    for (popup, location) in popups {
        collect(popup.wl_surface(), location - popup.geometry().loc, scale, &canvas.drawn, &mut layers);
    }
    for (surface, offset) in extra {
        collect(surface, *offset, scale, &canvas.drawn, &mut layers);
    }

    let same_layout = canvas.width == width
        && canvas.height == height
        && (canvas.scale - scale).abs() < 1e-6
        && canvas.drawn.len() == layers.len()
        && canvas.drawn.iter().zip(&layers).all(|((s, a, _), l)| *s == l.surface && *a == l.area);
    let bounds = Area { x0: 0, y0: 0, x1: width as i32, y1: height as i32 };
    let damage = if same_layout {
        let mut areas: Vec<Area> = Vec::new();
        for layer in &layers {
            match &layer.damage {
                None => areas.extend(layer.area.intersect(bounds)),
                Some(rects) => {
                    for r in rects {
                        if let Some(a) = buffer_to_canvas(layer, *r).and_then(|a| a.intersect(layer.area)).and_then(|a| a.intersect(bounds))
                        {
                            areas.push(a);
                        }
                    }
                }
            }
        }
        Some(merge(areas))
    } else {
        resize(canvas, width, height, scale);
        None
    };
    canvas.drawn = layers.iter().map(|l| (l.surface.clone(), l.area, l.commit)).collect();

    let areas = damage.clone().unwrap_or_else(|| vec![bounds]);
    for area in &areas {
        clear(canvas, *area);
        for layer in &layers {
            if let Some(clip) = layer.area.intersect(*area) {
                draw(layer, clip, canvas);
            }
        }
    }
    damage.map(|areas| areas.into_iter().map(Area::rect).collect())
}

/// Draws one surface tree whole into a canvas of its own size, for a cursor
/// image. Returns false when it has no buffer.
pub(crate) fn compose_surface(surface: &WlSurface, canvas: &mut Canvas) -> bool {
    let scale = density(surface);
    let Some(size) = view_of_surface(surface).map(|v| v.dst) else { return false };
    let width = ((f64::from(size.w.max(1)) * scale).round() as u32).max(1);
    let height = ((f64::from(size.h.max(1)) * scale).round() as u32).max(1);
    resize(canvas, width, height, scale);
    let mut layers = Vec::new();
    collect(surface, Point::from((0, 0)), scale, &[], &mut layers);
    let bounds = Area { x0: 0, y0: 0, x1: width as i32, y1: height as i32 };
    for layer in &layers {
        if let Some(clip) = layer.area.intersect(bounds) {
            draw(layer, clip, canvas);
        }
    }
    canvas.drawn.clear();
    true
}

fn resize(canvas: &mut Canvas, width: u32, height: u32, scale: f64) {
    canvas.width = width;
    canvas.height = height;
    canvas.scale = scale;
    canvas.pixels.clear();
    canvas.pixels.resize(width as usize * height as usize * 4, 0);
    canvas.drawn.clear();
}

/// Pixels per logical unit of the surface's buffer, 1 to 4, rounded to
/// 1/120 like `wp_fractional_scale_v1`.
fn density(surface: &WlSurface) -> f64 {
    let Some((view, buffer_scale)) = smithay::wayland::compositor::with_states(surface, |states| {
        let data = states.data_map.get::<RendererSurfaceStateUserData>()?.lock().unwrap();
        Some((data.view()?, data.buffer_scale()))
    }) else {
        return 1.0;
    };
    if view.dst.w <= 0 {
        return 1.0;
    }
    let d = view.src.size.w * f64::from(buffer_scale.max(1)) / f64::from(view.dst.w);
    ((d * 120.0).round() / 120.0).clamp(1.0, 4.0)
}

fn view_of(states: &SurfaceData) -> Option<SurfaceView> {
    states.data_map.get::<RendererSurfaceStateUserData>().and_then(|d| d.lock().unwrap().view())
}

fn view_of_surface(surface: &WlSurface) -> Option<SurfaceView> {
    smithay::wayland::compositor::with_states(surface, view_of)
}

/// Adds the buffers of a surface and its subsurfaces, with the surface
/// origin at `origin` (logical units, relative to the image).
fn collect(surface: &WlSurface, origin: Point<i32, Logical>, scale: f64, drawn: &[(WlSurface, Area, CommitCounter)], out: &mut Vec<Layer>) {
    with_surface_tree_downward(
        surface,
        origin,
        |_, states, loc| match view_of(states) {
            Some(view) => TraversalAction::DoChildren(*loc + view.offset),
            // An unmapped parent hides its children.
            None => TraversalAction::SkipChildren,
        },
        |s, states, loc| {
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else { return };
            let data = data.lock().unwrap();
            let (Some(view), Some(buffer)) = (data.view(), data.buffer()) else { return };
            let at = *loc + view.offset;
            let px = |v: i32| (f64::from(v) * scale).round() as i32;
            let area = Area { x0: px(at.x), y0: px(at.y), x1: px(at.x + view.dst.w), y1: px(at.y + view.dst.h) };
            let previous = drawn.iter().find(|(d, _, _)| d == s).map(|(_, _, c)| *c);
            let damage = previous.map(|c| data.damage_since(Some(c)).iter().copied().collect());
            out.push(Layer {
                surface: s.clone(),
                buffer: (**buffer).clone(),
                buffer_scale: data.buffer_scale(),
                view,
                area,
                commit: data.current_commit(),
                damage,
            });
        },
        |_, _, _| true,
    );
}

/// A damaged buffer rectangle as canvas pixels, a pixel larger on every
/// side because scaling spreads it.
fn buffer_to_canvas(layer: &Layer, r: smithay::utils::Rectangle<i32, smithay::utils::Buffer>) -> Option<Area> {
    let bs = f64::from(layer.buffer_scale.max(1));
    let src = layer.view.src;
    if src.size.w <= 0.0 || src.size.h <= 0.0 {
        return None;
    }
    // Canvas pixels per logical unit of the buffer's source rectangle.
    let (sx, sy) = (f64::from(layer.area.x1 - layer.area.x0) / src.size.w, f64::from(layer.area.y1 - layer.area.y0) / src.size.h);
    let x0 = f64::from(layer.area.x0) + (f64::from(r.loc.x) / bs - src.loc.x) * sx;
    let y0 = f64::from(layer.area.y0) + (f64::from(r.loc.y) / bs - src.loc.y) * sy;
    let x1 = f64::from(layer.area.x0) + (f64::from(r.loc.x + r.size.w) / bs - src.loc.x) * sx;
    let y1 = f64::from(layer.area.y0) + (f64::from(r.loc.y + r.size.h) / bs - src.loc.y) * sy;
    let a = Area { x0: x0.floor() as i32 - 1, y0: y0.floor() as i32 - 1, x1: x1.ceil() as i32 + 1, y1: y1.ceil() as i32 + 1 };
    (a.x0 < a.x1 && a.y0 < a.y1).then_some(a)
}

/// Removes regions inside others and joins too many into one box.
fn merge(mut areas: Vec<Area>) -> Vec<Area> {
    areas.sort_by_key(|a| (a.y0, a.x0));
    areas.dedup();
    let inside = |a: &Area, b: &Area| a != b && b.x0 <= a.x0 && b.y0 <= a.y0 && a.x1 <= b.x1 && a.y1 <= b.y1;
    let kept: Vec<Area> = areas.iter().filter(|a| !areas.iter().any(|b| inside(a, b))).copied().collect();
    if kept.len() > MAX_RECTS { kept.iter().copied().reduce(Area::union).into_iter().collect() } else { kept }
}

fn clear(canvas: &mut Canvas, area: Area) {
    let width = canvas.width as usize;
    for y in area.y0..area.y1 {
        let start = (y as usize * width + area.x0 as usize) * 4;
        let end = start + (area.x1 - area.x0) as usize * 4;
        canvas.pixels[start..end].fill(0);
    }
}

fn draw(layer: &Layer, clip: Area, canvas: &mut Canvas) {
    let result = with_buffer_contents(&layer.buffer, |ptr, len, info| {
        // SAFETY: smithay hands out the mapped pool, `len` bytes from `ptr`, for the duration of this call.
        let memory = unsafe { std::slice::from_raw_parts(ptr, len) };
        blit(memory, info, layer.buffer_scale, layer.view, layer.area, clip, canvas);
    });
    if let Err(e) = result {
        tracing::debug!("cannot read a surface buffer: {e:?}");
    }
}

/// Copies one buffer into the canvas. `view` crops it (wl_surface buffer
/// scale and wp_viewporter), `area` is where it lands in canvas pixels and
/// `clip` the part of the canvas to draw. Only the normal buffer transform
/// is handled. Scaled buffers are sampled nearest neighbour.
fn blit(memory: &[u8], info: BufferData, buffer_scale: i32, view: SurfaceView, area: Area, clip: Area, canvas: &mut Canvas) {
    let opaque = match info.format {
        Format::Argb8888 => false,
        Format::Xrgb8888 => true,
        other => {
            tracing::debug!("unsupported shm format {other:?}");
            return;
        }
    };
    let (bw, bh, stride, offset) = (info.width, info.height, info.stride, info.offset);
    if bw <= 0 || bh <= 0 || stride < bw * 4 || offset < 0 {
        return;
    }
    let needed = offset as usize + stride as usize * (bh as usize - 1) + bw as usize * 4;
    if needed > memory.len() {
        return;
    }
    let (dx0, dy0) = (area.x0, area.y0);
    let (dw, dh) = (area.x1 - area.x0, area.y1 - area.y0);
    if dw <= 0 || dh <= 0 {
        return;
    }
    // Source in buffer pixels.
    let bs = f64::from(buffer_scale.max(1));
    let (sx0, sy0) = (view.src.loc.x * bs, view.src.loc.y * bs);
    let (sw, sh) = (view.src.size.w * bs, view.src.size.h * bs);
    let bounds = Area { x0: 0, y0: 0, x1: canvas.width as i32, y1: canvas.height as i32 };
    let Some(target) = area.intersect(clip).and_then(|a| a.intersect(bounds)) else { return };
    let one_to_one = (sw - f64::from(dw)).abs() < 0.01 && (sh - f64::from(dh)).abs() < 0.01 && sx0.fract() == 0.0 && sy0.fract() == 0.0;
    let map_x = |x: i32| -> usize {
        let sx = if one_to_one { sx0 as i32 + (x - dx0) } else { (sx0 + (f64::from(x - dx0) + 0.5) * sw / f64::from(dw)) as i32 };
        sx.clamp(0, bw - 1) as usize
    };
    let columns: Vec<usize> = (target.x0..target.x1).map(map_x).collect();
    let width = canvas.width as usize;
    for y in target.y0..target.y1 {
        let sy = if one_to_one { sy0 as i32 + (y - dy0) } else { (sy0 + (f64::from(y - dy0) + 0.5) * sh / f64::from(dh)) as i32 };
        let row = offset as usize + sy.clamp(0, bh - 1) as usize * stride as usize;
        let out = (y as usize * width + target.x0 as usize) * 4;
        let out = &mut canvas.pixels[out..out + columns.len() * 4];
        for (dst, &sx) in out.as_chunks_mut::<4>().0.iter_mut().zip(&columns) {
            let s = row + sx * 4;
            // wl_shm formats are little endian, so ARGB8888 is B, G, R, A in memory, premultiplied.
            let (b, g, r) = (memory[s], memory[s + 1], memory[s + 2]);
            let a = if opaque { 255 } else { memory[s + 3] };
            match a {
                255 => dst.copy_from_slice(&[r, g, b, 255]),
                0 => {}
                _ => {
                    let keep = 255 - u32::from(a);
                    let over = |src: u8, dst: u8| (u32::from(src) + (u32::from(dst) * keep + 127) / 255).min(255) as u8;
                    dst[0] = over(r, dst[0]);
                    dst[1] = over(g, dst[1]);
                    dst[2] = over(b, dst[2]);
                    dst[3] = over(a, dst[3]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::{Rectangle, Size};

    fn info(w: i32, h: i32, format: Format) -> BufferData {
        BufferData { offset: 0, width: w, height: h, stride: w * 4, format }
    }

    fn view(w: i32, h: i32) -> SurfaceView {
        SurfaceView {
            src: Rectangle::from_size(Size::from((f64::from(w), f64::from(h)))),
            dst: Size::from((w, h)),
            offset: Point::from((0, 0)),
        }
    }

    fn canvas(w: u32, h: u32) -> Canvas {
        Canvas { width: w, height: h, pixels: vec![0; (w * h * 4) as usize], ..Canvas::default() }
    }

    fn area(x0: i32, y0: i32, x1: i32, y1: i32) -> Area {
        Area { x0, y0, x1, y1 }
    }

    const ALL: Area = Area { x0: -10_000, y0: -10_000, x1: 10_000, y1: 10_000 };

    #[test]
    fn opaque_pixels_are_swizzled_to_rgba() {
        // One blue, one red pixel in XRGB8888 (memory order B, G, R, X).
        let memory = [255, 0, 0, 7, 0, 0, 255, 7];
        let mut c = canvas(2, 1);
        blit(&memory, info(2, 1, Format::Xrgb8888), 1, view(2, 1), area(0, 0, 2, 1), ALL, &mut c);
        assert_eq!(c.pixels, [0, 0, 255, 255, 255, 0, 0, 255]);
    }

    #[test]
    fn translucent_pixels_blend_over_what_is_below() {
        let mut c = canvas(1, 1);
        c.pixels.copy_from_slice(&[0, 0, 200, 255]);
        // Half transparent premultiplied red.
        let memory = [0, 0, 128, 128];
        blit(&memory, info(1, 1, Format::Argb8888), 1, view(1, 1), area(0, 0, 1, 1), ALL, &mut c);
        assert_eq!(c.pixels, [128, 0, 100, 255]);
    }

    #[test]
    fn buffers_are_placed_clipped_and_scaled() {
        // A 1x1 logical surface with a scale 1 buffer at density 2 covers 2x2 pixels.
        let memory = [10, 20, 30, 255];
        let mut c = canvas(3, 3);
        blit(&memory, info(1, 1, Format::Argb8888), 1, view(1, 1), area(2, 2, 4, 4), ALL, &mut c);
        let px = |c: &Canvas, x: usize, y: usize| c.pixels[(y * 3 + x) * 4..(y * 3 + x) * 4 + 4].to_vec();
        assert_eq!(px(&c, 0, 0), [0, 0, 0, 0], "nothing left of and above the surface");
        assert_eq!(px(&c, 2, 2), [30, 20, 10, 255], "the part that fits is drawn, the rest is clipped");
        // A surface partly left of the canvas.
        let memory = [1, 1, 1, 255, 2, 2, 2, 255];
        let mut c = canvas(1, 1);
        blit(&memory, info(2, 1, Format::Argb8888), 1, view(2, 1), area(-1, 0, 1, 1), ALL, &mut c);
        assert_eq!(c.pixels, [2, 2, 2, 255]);
    }

    #[test]
    fn only_the_clip_is_drawn() {
        let memory = [9, 9, 9, 255, 9, 9, 9, 255, 9, 9, 9, 255];
        let mut c = canvas(3, 1);
        blit(&memory, info(3, 1, Format::Argb8888), 1, view(3, 1), area(0, 0, 3, 1), area(1, 0, 2, 1), &mut c);
        assert_eq!(c.pixels, [0, 0, 0, 0, 9, 9, 9, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn a_fractional_buffer_maps_one_to_one() {
        // 5 buffer pixels shown as 4 logical units at density 1.25 land on 5 canvas pixels unscaled.
        let memory: Vec<u8> = (0..5u8).flat_map(|i| [i, i, i, 255]).collect();
        let v = SurfaceView { src: Rectangle::from_size(Size::from((5.0, 1.0))), dst: Size::from((4, 1)), offset: Point::from((0, 0)) };
        let mut c = canvas(5, 1);
        blit(&memory, info(5, 1, Format::Argb8888), 1, v, area(0, 0, 5, 1), ALL, &mut c);
        let reds: Vec<u8> = c.pixels.chunks(4).map(|p| p[0]).collect();
        assert_eq!(reds, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn short_buffers_are_ignored() {
        let mut c = canvas(2, 2);
        blit(&[0; 8], info(2, 2, Format::Argb8888), 1, view(2, 2), area(0, 0, 2, 2), ALL, &mut c);
        assert!(c.pixels.iter().all(|&b| b == 0));
    }

    #[test]
    fn damage_regions_are_merged() {
        let inner = area(2, 2, 4, 4);
        let outer = area(0, 0, 10, 10);
        assert_eq!(merge(vec![inner, outer, outer]), [outer], "regions inside others go");
        let many: Vec<Area> = (0..20).map(|i| area(i * 3, 0, i * 3 + 1, 1)).collect();
        assert_eq!(merge(many), [area(0, 0, 58, 1)], "too many become one box");
    }
}
