//! A cube map seen in 3D, rendered on the CPU: from inside (like a skybox) or wrapped on a sphere.
//!
//! Faces are in Direct3D order (+X, -X, +Y, -Y, +Z, -Z) with D3D's cube addressing: a direction picks the
//! face of its largest component, and the other two give the position on that face.

use crate::bcn::Rgba;
use rayon::prelude::*;

#[derive(Clone, Copy, PartialEq)]
pub struct View {
    /// turn left/right and up/down, in radians
    pub yaw: f32,
    pub pitch: f32,
    /// field of view (inside) or zoom (sphere), in radians
    pub fov: f32,
    pub sphere: bool,
    /// the cube map's world has Z up (as in RAGE games) rather than Direct3D's Y up
    pub z_up: bool,
}

impl Default for View {
    fn default() -> View {
        View { yaw: 0.0, pitch: 0.0, fov: 90f32.to_radians(), sphere: false, z_up: true }
    }
}

/// Colour of the cube map in direction (x, y, z) (y up), with bilinear filtering inside each face.
fn sample(faces: &[Rgba; 6], x: f32, y: f32, z: f32) -> [u8; 4] {
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let (face, ma, sc, tc) = if ax >= ay && ax >= az {
        if x > 0.0 { (0, ax, -z, -y) } else { (1, ax, z, -y) }
    } else if ay >= az {
        if y > 0.0 { (2, ay, x, z) } else { (3, ay, x, -z) }
    } else if z > 0.0 {
        (4, az, x, -y)
    } else {
        (5, az, -x, -y)
    };
    let f = &faces[face];
    let u = ((sc / ma + 1.0) * 0.5 * f.w as f32 - 0.5).clamp(0.0, (f.w - 1) as f32);
    let v = ((tc / ma + 1.0) * 0.5 * f.h as f32 - 0.5).clamp(0.0, (f.h - 1) as f32);
    let (x0, y0) = (u as usize, v as usize);
    let (x1, y1) = ((x0 + 1).min(f.w - 1), (y0 + 1).min(f.h - 1));
    let (fx, fy) = (u - x0 as f32, v - y0 as f32);
    let px = |xx: usize, yy: usize, c: usize| f.px[(yy * f.w + xx) * 4 + c] as f32;
    let mut out = [0u8; 4];
    for (c, o) in out.iter_mut().enumerate() {
        let top = px(x0, y0, c) * (1.0 - fx) + px(x1, y0, c) * fx;
        let bot = px(x0, y1, c) * (1.0 - fx) + px(x1, y1, c) * fx;
        *o = (top * (1.0 - fy) + bot * fy + 0.5) as u8;
    }
    out
}

/// Render `w` x `h` RGBA pixels. Outside the sphere is left transparent.
pub fn render(faces: &[Rgba; 6], view: View, w: usize, h: usize) -> Vec<u8> {
    let (sy, cy, sp, cp) = (view.yaw.sin(), view.yaw.cos(), view.pitch.sin(), view.pitch.cos());
    // camera axes: forward looks along +Z at yaw 0
    let fwd = [sy * cp, sp, cy * cp];
    let right = [cy, 0.0, -sy];
    let up = [-sy * sp, cp, -cy * sp];
    let aspect = w as f32 / h as f32;
    let scale = (view.fov * 0.5).tan();
    let mut px = vec![0u8; w * h * 4];
    px.par_chunks_mut(w * 4).enumerate().for_each(|(j, row)| {
        let ny = 1.0 - 2.0 * (j as f32 + 0.5) / h as f32;
        for i in 0..w {
            let nx = 2.0 * (i as f32 + 0.5) / w as f32 - 1.0;
            let (a, b, c) = if view.sphere {
                // a ball filling the view (scaled by the zoom); its surface points in the direction it shows
                let k = scale.max(0.2);
                let (sx, syy) = (nx * aspect * k, ny * k);
                let r2 = sx * sx + syy * syy;
                if r2 > 1.0 {
                    continue;
                }
                let sz = -(1.0 - r2).sqrt(); // facing the viewer
                (sx, syy, sz)
            } else {
                (nx * aspect * scale, ny * scale, 1.0)
            };
            let d = [
                right[0] * a + up[0] * b + fwd[0] * c,
                right[1] * a + up[1] * b + fwd[1] * c,
                right[2] * a + up[2] * b + fwd[2] * c,
            ];
            // the camera works Y-up; for a Z-up cube map turn its up (+Y) to +Z
            let col = if view.z_up { sample(faces, d[0], -d[2], d[1]) } else { sample(faces, d[0], d[1], d[2]) };
            row[i * 4..i * 4 + 4].copy_from_slice(&[col[0], col[1], col[2], 255]);
        }
    });
    px
}
