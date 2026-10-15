//! Low-poly modelling for meerora figures and spaces (D4, D20).
//!
//! Plain Rust, no GPU. Units are centimetres and y points up. Meshes are flat
//! shaded, so every triangle owns its three vertices and carries the face
//! normal. Front faces wind counter-clockwise seen from outside.
//!
//! Colours stored in meshes are linear RGB. Scripts and the design tokens use
//! `#RRGGBB` sRGB values, which [`srgb_hex`] converts.

pub mod prim;

pub use glam::{Mat3, Mat4, Quat, Vec2, Vec3};

/// One vertex as uploaded to the GPU. 40 bytes, no padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    /// Linear RGB.
    pub color: [f32; 3],
    /// 0 means lit only by lights. 1 glows at full colour (eyes, bulbs, screens).
    pub emissive: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

const WHITE: [f32; 3] = [1.0, 1.0, 1.0];

impl Mesh {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Adds one flat triangle, counter-clockwise seen from outside. Degenerate
    /// triangles are skipped.
    pub fn push_triangle(&mut self, a: Vec3, b: Vec3, c: Vec3) {
        let n = (b - a).cross(c - a);
        if n.length_squared() <= f32::EPSILON * f32::EPSILON {
            return;
        }
        let normal = n.normalize().to_array();
        let base = self.vertices.len() as u32;
        for p in [a, b, c] {
            self.vertices.push(Vertex { position: p.to_array(), normal, color: WHITE, emissive: 0.0 });
        }
        self.indices.extend_from_slice(&[base, base + 1, base + 2]);
    }

    /// Adds a flat quad as two triangles. Corners counter-clockwise seen from outside.
    pub fn push_quad(&mut self, a: Vec3, b: Vec3, c: Vec3, d: Vec3) {
        self.push_triangle(a, b, c);
        self.push_triangle(a, c, d);
    }

    pub fn append(&mut self, other: &Mesh) {
        let base = self.vertices.len() as u32;
        self.vertices.extend_from_slice(&other.vertices);
        self.indices.extend(other.indices.iter().map(|i| i + base));
    }

    /// Transforms positions and normals. A mirroring transform flips the
    /// winding so front faces stay counter-clockwise.
    pub fn transform(&mut self, m: Mat4) {
        let normal_m = Mat3::from_mat4(m).inverse().transpose();
        for v in &mut self.vertices {
            v.position = m.transform_point3(Vec3::from(v.position)).to_array();
            v.normal = (normal_m * Vec3::from(v.normal)).normalize_or_zero().to_array();
        }
        if m.determinant() < 0.0 {
            for tri in self.indices.as_chunks_mut::<3>().0 {
                tri.swap(1, 2);
            }
        }
    }

    pub fn transformed(&self, m: Mat4) -> Mesh {
        let mut out = self.clone();
        out.transform(m);
        out
    }

    pub fn bounds(&self) -> Aabb {
        let mut b = Aabb::EMPTY;
        for v in &self.vertices {
            b.extend(Vec3::from(v.position));
        }
        b
    }

    /// Sets colour and emission on every vertex.
    pub fn paint(&mut self, material: &Material) {
        for v in &mut self.vertices {
            v.color = material.color;
            v.emissive = material.emissive;
        }
    }

    pub fn painted(mut self, material: &Material) -> Mesh {
        self.paint(material);
        self
    }
}

/// Axis-aligned bounding box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub const EMPTY: Aabb = Aabb { min: Vec3::splat(f32::INFINITY), max: Vec3::splat(f32::NEG_INFINITY) };

    pub fn new(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    pub fn is_empty(&self) -> bool {
        self.min.x > self.max.x || self.min.y > self.max.y || self.min.z > self.max.z
    }

    pub fn extend(&mut self, p: Vec3) {
        self.min = self.min.min(p);
        self.max = self.max.max(p);
    }

    pub fn union(self, other: Aabb) -> Aabb {
        Aabb { min: self.min.min(other.min), max: self.max.max(other.max) }
    }

    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    pub fn size(&self) -> Vec3 {
        self.max - self.min
    }

    /// Box around the eight transformed corners.
    pub fn transformed(&self, m: Mat4) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        let mut out = Aabb::EMPTY;
        for i in 0..8 {
            let p = Vec3::new(
                if i & 1 == 0 { self.min.x } else { self.max.x },
                if i & 2 == 0 { self.min.y } else { self.max.y },
                if i & 4 == 0 { self.min.z } else { self.max.z },
            );
            out.extend(m.transform_point3(p));
        }
        out
    }

    /// Distance along the ray to the first hit, used for picking.
    pub fn ray_hit(&self, origin: Vec3, dir: Vec3) -> Option<f32> {
        let inv = dir.recip();
        let t1 = (self.min - origin) * inv;
        let t2 = (self.max - origin) * inv;
        let t_near = t1.min(t2).max_element();
        let t_far = t1.max(t2).min_element();
        (t_far >= t_near.max(0.0)).then_some(t_near.max(0.0))
    }
}

/// Surface finish named in scripts (`.material("felt", "#D9A441")`). The
/// milestone 1 renderer shades every finish the same way. The name is kept so
/// later renderers can tell felt from enamel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Finish {
    #[default]
    Matte,
    Enamel,
    Ceramic,
    Felt,
    Glaze,
    Paper,
    Brass,
    Metal,
    Wood,
    Leather,
    Glass,
    Plastic,
    Stone,
    Foliage,
}

impl Finish {
    pub fn parse(name: &str) -> Option<Finish> {
        Some(match name.to_ascii_lowercase().as_str() {
            "matte" => Finish::Matte,
            "enamel" => Finish::Enamel,
            "ceramic" => Finish::Ceramic,
            "felt" => Finish::Felt,
            "glaze" => Finish::Glaze,
            "paper" => Finish::Paper,
            "brass" => Finish::Brass,
            "metal" => Finish::Metal,
            "wood" => Finish::Wood,
            "leather" => Finish::Leather,
            "glass" => Finish::Glass,
            "plastic" => Finish::Plastic,
            "stone" => Finish::Stone,
            "foliage" => Finish::Foliage,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Material {
    /// Linear RGB.
    pub color: [f32; 3],
    pub finish: Finish,
    pub emissive: f32,
}

impl Default for Material {
    fn default() -> Self {
        Self { color: WHITE, finish: Finish::Matte, emissive: 0.0 }
    }
}

impl Material {
    pub fn from_hex(hex: &str) -> Result<Material, GeomError> {
        Ok(Material { color: srgb_hex(hex)?, ..Default::default() })
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum GeomError {
    #[error("invalid colour {0:?}, expected #RRGGBB")]
    BadColor(String),
}

pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// Parses `#RRGGBB`, `RRGGBB` or `#RGB` into linear RGB.
pub fn srgb_hex(hex: &str) -> Result<[f32; 3], GeomError> {
    let bad = || GeomError::BadColor(hex.to_string());
    let s = hex.strip_prefix('#').unwrap_or(hex);
    let digits: Vec<u8> = match s.len() {
        6 => (0..3).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)).collect::<Result<_, _>>().map_err(|_| bad())?,
        3 => s.chars().map(|c| c.to_digit(16).map(|d| (d * 17) as u8)).collect::<Option<_>>().ok_or_else(bad)?,
        _ => return Err(bad()),
    };
    Ok([0, 1, 2].map(|i| srgb_to_linear(digits[i] as f32 / 255.0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours() {
        assert_eq!(srgb_hex("#FFFFFF").unwrap(), [1.0, 1.0, 1.0]);
        assert_eq!(srgb_hex("000").unwrap(), [0.0, 0.0, 0.0]);
        assert!((srgb_hex("#808080").unwrap()[0] - 0.2158).abs() < 1e-3);
        assert!(srgb_hex("#12345").is_err());
        assert!(srgb_hex("#GGGGGG").is_err());
    }

    #[test]
    fn mirror_keeps_front_faces() {
        let mut m = Mesh::new();
        m.push_triangle(Vec3::ZERO, Vec3::X, Vec3::Y);
        m.transform(Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0)));
        let v: Vec<Vec3> = m.indices.iter().map(|&i| Vec3::from(m.vertices[i as usize].position)).collect();
        let n = (v[1] - v[0]).cross(v[2] - v[0]).normalize();
        assert!((n - Vec3::from(m.vertices[0].normal)).length() < 1e-5);
    }

    #[test]
    fn ray_hits_box() {
        let b = Aabb::new(Vec3::splat(-1.0), Vec3::splat(1.0));
        assert_eq!(b.ray_hit(Vec3::new(0.0, 0.0, -5.0), Vec3::Z), Some(4.0));
        assert_eq!(b.ray_hit(Vec3::new(3.0, 0.0, -5.0), Vec3::Z), None);
    }
}
