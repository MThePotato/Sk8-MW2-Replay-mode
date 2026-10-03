//! Procedurally drawn replay-UI iconography. No art files ship with the game:
//! each glyph is rasterized here at 4x and box-downsampled into an `Image`.
//!
//! Glyphs render white with an alpha coverage mask, so `ImageNode::color`
//! tints them (idle, active camera, cut camera, marker colours).

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

pub(crate) const TINT_IDLE: Color = Color::srgb(0.55, 0.60, 0.66);
pub(crate) const TINT_ACTIVE: Color = Color::srgb(1.0, 0.85, 0.3);
pub(crate) const TINT_CUT: Color = Color::srgb(0.35, 0.80, 1.0);
pub(crate) const TINT_KEYFRAME: Color = Color::srgb(1.0, 0.85, 0.2);
pub(crate) const TINT_RAMP: Color = Color::srgb(1.0, 0.55, 0.15);

pub(crate) struct ReplayIcons {
    pub keyframe: Handle<Image>,
    pub cut: Handle<Image>,
    pub ramp: Handle<Image>,
    pub cams: Vec<Handle<Image>>,
}

/// Supersampling factor: shapes fill the canvas at 4x and are box-downsampled
/// to the final size, which smooths the edges of these small glyphs.
const SS: usize = 4;

struct Canvas {
    ws: usize,
    hs: usize,
    cov: Vec<f32>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self {
            ws: w * SS,
            hs: h * SS,
            cov: vec![0.0; w * SS * h * SS],
        }
    }

    fn plot(&mut self, x: i32, y: i32) {
        if x >= 0 && y >= 0 && (x as usize) < self.ws && (y as usize) < self.hs {
            self.cov[y as usize * self.ws + x as usize] = 1.0;
        }
    }

    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
        for y in (y0 * SS as f32) as i32..(y1 * SS as f32) as i32 {
            for x in (x0 * SS as f32) as i32..(x1 * SS as f32) as i32 {
                self.plot(x, y);
            }
        }
    }

    fn disc(&mut self, cx: f32, cy: f32, r: f32) {
        self.annulus(cx, cy, r, 0.0);
    }

    fn annulus(&mut self, cx: f32, cy: f32, r_out: f32, r_in: f32) {
        let (cx, cy) = (cx * SS as f32, cy * SS as f32);
        let (ro, ri) = (r_out * SS as f32, r_in * SS as f32);
        let (x0, x1) = ((cx - ro) as i32, (cx + ro) as i32 + 1);
        let (y0, y1) = ((cy - ro) as i32, (cy + ro) as i32 + 1);
        for y in y0..y1 {
            for x in x0..x1 {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let d2 = dx * dx + dy * dy;
                if d2 <= ro * ro && d2 >= ri * ri {
                    self.plot(x, y);
                }
            }
        }
    }

    /// Filled triangle. Edges are tested with a consistent sign so either
    /// winding works.
    fn tri(&mut self, a: (f32, f32), b: (f32, f32), c: (f32, f32)) {
        let s = SS as f32;
        let (a, b, c) = ((a.0 * s, a.1 * s), (b.0 * s, b.1 * s), (c.0 * s, c.1 * s));
        let x0 = a.0.min(b.0).min(c.0) as i32;
        let x1 = a.0.max(b.0).max(c.0) as i32 + 1;
        let y0 = a.1.min(b.1).min(c.1) as i32;
        let y1 = a.1.max(b.1).max(c.1) as i32 + 1;
        let edge = |p: (f32, f32), q: (f32, f32), x: f32, y: f32| {
            (q.0 - p.0) * (y - p.1) - (q.1 - p.1) * (x - p.0)
        };
        for y in y0..y1 {
            for x in x0..x1 {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let e0 = edge(a, b, fx, fy);
                let e1 = edge(b, c, fx, fy);
                let e2 = edge(c, a, fx, fy);
                if (e0 >= 0.0 && e1 >= 0.0 && e2 >= 0.0) || (e0 <= 0.0 && e1 <= 0.0 && e2 <= 0.0)
                {
                    self.plot(x, y);
                }
            }
        }
    }

    fn to_image(self) -> Image {
        let w = self.ws / SS;
        let h = self.hs / SS;
        let mut data = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0.0;
                for sy in 0..SS {
                    for sx in 0..SS {
                        sum += self.cov[(y * SS + sy) * self.ws + x * SS + sx];
                    }
                }
                let alpha = ((sum / (SS * SS) as f32) * 255.0).round() as u8;
                data.extend_from_slice(&[255, 255, 255, alpha]);
            }
        }
        Image::new(
            Extent3d {
                width: w as u32,
                height: h as u32,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            data,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
        )
    }
}

/// Rasterizes every glyph once and hands back the handles.
pub(crate) fn build(images: &mut Assets<Image>) -> ReplayIcons {
    ReplayIcons {
        keyframe: images.add(diamond(12.0, 12.0)),
        cut: images.add(cut_flag(12.0, 12.0)),
        ramp: images.add(ramp_dot(8.0)),
        cams: (0..14).map(|cam| images.add(cam_glyph(cam))).collect(),
    }
}

fn diamond(w: f32, h: f32) -> Image {
    let mut c = Canvas::new(w as usize, h as usize);
    let (cx, cy) = (w * 0.5, h * 0.5);
    c.tri((cx, cy - h * 0.5 + 0.5), (cx + w * 0.5 - 0.5, cy), (cx, cy + h * 0.5 - 0.5));
    c.tri((cx, cy - h * 0.5 + 0.5), (cx - w * 0.5 + 0.5, cy), (cx, cy + h * 0.5 - 0.5));
    c.to_image()
}

fn cut_flag(w: f32, h: f32) -> Image {
    let mut c = Canvas::new(w as usize, h as usize);
    c.rect(1.6, 0.5, 3.1, h - 0.5);
    c.tri((3.1, 0.5), (w - 0.5, 3.2), (3.1, 6.0));
    c.to_image()
}

fn ramp_dot(size: f32) -> Image {
    let mut c = Canvas::new(size as usize, size as usize);
    c.disc(size * 0.5, size * 0.5, size * 0.42);
    c.to_image()
}

/// The camera-strip glyph for one replay camera index. Kept in sync with
/// `cam_name`/`replay_cam_name`.
fn cam_glyph(cam: u8) -> Image {
    const W: usize = 24;
    const H: usize = 20;
    let mut c = Canvas::new(W, H);
    match cam {
        0 => {
            // recorded: camera body with a lens and viewfinder bump
            c.rect(2.0, 6.0, 22.0, 17.0);
            c.rect(8.0, 3.5, 16.0, 6.0);
            c.annulus(12.0, 11.5, 4.6, 2.6);
        }
        1 => {
            // orbit: ring with a satellite dot
            c.annulus(12.0, 10.5, 7.6, 5.6);
            c.disc(17.6, 4.9, 2.2);
        }
        2 => c.tri((15.5, 4.0), (15.5, 16.0), (4.0, 10.0)),
        3 => c.tri((8.5, 4.0), (8.5, 16.0), (20.0, 10.0)),
        4 => c.tri((4.0, 5.5), (20.0, 5.5), (12.0, 16.5)),
        5 => {
            // chase: double chevron
            c.tri((5.0, 4.0), (5.0, 16.0), (13.0, 10.0));
            c.tri((11.5, 4.0), (11.5, 16.0), (19.5, 10.0));
        }
        6 => {
            // free: move cross with arrow tips
            c.rect(10.8, 3.5, 13.2, 16.5);
            c.rect(3.5, 8.8, 20.5, 11.2);
            c.tri((12.0, 1.2), (9.4, 4.6), (14.6, 4.6));
            c.tri((12.0, 18.8), (9.4, 15.4), (14.6, 15.4));
            c.tri((1.2, 10.0), (4.6, 7.4), (4.6, 12.6));
            c.tri((22.8, 10.0), (19.4, 7.4), (19.4, 12.6));
        }
        7 => return diamond(W as f32, H as f32),
        8 => {
            // firstperson: eye (ring with pupil)
            c.annulus(12.0, 10.0, 6.8, 5.0);
            c.disc(12.0, 10.0, 2.4);
        }
        9 => {
            // fisheye: wide lens ring with a horizon line
            c.annulus(12.0, 10.0, 8.0, 6.6);
            c.rect(4.0, 9.2, 20.0, 10.8);
        }
        10 => {
            // tripod: head, centre column and two legs
            c.rect(9.0, 3.0, 15.0, 6.0);
            c.rect(11.0, 6.0, 13.0, 10.0);
            c.tri((11.0, 8.0), (4.0, 18.0), (8.0, 18.0));
            c.tri((13.0, 8.0), (16.0, 18.0), (20.0, 18.0));
        }
        11 => {
            // follow: leader dot with a fading trail
            c.disc(16.5, 10.0, 3.6);
            c.disc(9.5, 10.0, 2.5);
            c.disc(4.5, 10.0, 1.5);
        }
        12 => {
            // body: head-and-torso silhouette
            c.disc(12.0, 4.6, 2.6);
            c.rect(8.4, 8.2, 15.6, 17.5);
            c.rect(4.6, 9.0, 8.4, 12.6);
            c.rect(15.6, 9.0, 19.4, 12.6);
        }
        _ => {
            // board: deck with two wheels
            c.rect(2.2, 8.0, 21.8, 10.2);
            c.disc(8.0, 13.4, 2.1);
            c.disc(16.0, 13.4, 2.1);
        }
    }
    c.to_image()
}
