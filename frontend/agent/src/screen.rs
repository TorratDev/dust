//! Screenshots of both screens (top above bottom, 256x384).

use crate::{Error, Session};
use dust_core::gpu::{Framebuffer, SCREEN_HEIGHT, SCREEN_WIDTH};

pub const SCREENSHOT_WIDTH: u32 = SCREEN_WIDTH as u32;
pub const SCREENSHOT_HEIGHT: u32 = 2 * SCREEN_HEIGHT as u32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImageFormat {
    /// 3 bytes per pixel, rows top to bottom.
    #[default]
    RawRgb,
    /// 8-bit RGB PNG.
    Png,
    /// 1 byte of luma per pixel (BT.601 weights), rows top to bottom.
    RawGray,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub data: Vec<u8>,
}

/// Framebuffer pixels are 0xAABBGGRR; the top screen comes first.
pub(crate) fn framebuffer_rgb(framebuffer: &Framebuffer) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(framebuffer.len() * framebuffer[0].len() * 3);
    for &pixel in framebuffer.iter().flatten() {
        rgb.extend_from_slice(&pixel.to_le_bytes()[..3]);
    }
    rgb
}

/// Averages `factor`x`factor` blocks (pixels past the last whole block are dropped), then
/// converts to `format` (`RawRgb` or `RawGray`).
fn scaled_pixels(framebuffer: &Framebuffer, gray: bool, factor: u32) -> (u32, u32, Vec<u8>) {
    let factor = factor.max(1) as usize;
    let (width, height) = (
        SCREENSHOT_WIDTH as usize / factor,
        SCREENSHOT_HEIGHT as usize / factor,
    );
    let rows: Vec<&[u32]> = framebuffer
        .iter()
        .flat_map(|screen| screen.chunks_exact(SCREEN_WIDTH))
        .collect();
    let channels = if gray { 1 } else { 3 };
    let mut out = Vec::with_capacity(width * height * channels);
    let area = (factor * factor) as u32;
    for by in 0..height {
        for bx in 0..width {
            let mut sum = [0u32; 3];
            for row in &rows[by * factor..(by + 1) * factor] {
                for &pixel in &row[bx * factor..(bx + 1) * factor] {
                    let [r, g, b, _] = pixel.to_le_bytes();
                    sum[0] += r as u32;
                    sum[1] += g as u32;
                    sum[2] += b as u32;
                }
            }
            let [r, g, b] = sum.map(|c| (c + area / 2) / area);
            if gray {
                out.push(((299 * r + 587 * g + 114 * b + 500) / 1000) as u8);
            } else {
                out.extend_from_slice(&[r as u8, g as u8, b as u8]);
            }
        }
    }
    (width as u32, height as u32, out)
}

pub(crate) fn encode_png(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut png_data = Vec::new();
    let mut encoder = png::Encoder::new(&mut png_data, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    // Writing to a Vec can't fail and the data length matches the header.
    let mut writer = encoder.write_header().expect("couldn't write PNG header");
    writer
        .write_image_data(rgb)
        .expect("couldn't write PNG data");
    writer.finish().expect("couldn't finish PNG");
    png_data
}

impl Session {
    /// The last finished frame (or, right after loading a state, the frame stored in it).
    pub fn framebuffer(&self) -> &Framebuffer {
        match &self.screen_override {
            Some(framebuffer) => framebuffer,
            None => self.emu.gpu.renderer_2d().framebuffer(),
        }
    }

    pub fn screenshot(&self, format: ImageFormat) -> Result<Image, Error> {
        if format == ImageFormat::RawGray {
            return self.observe_screen(true, 1);
        }
        let rgb = framebuffer_rgb(self.framebuffer());
        Ok(Image {
            width: SCREENSHOT_WIDTH,
            height: SCREENSHOT_HEIGHT,
            format,
            data: match format {
                ImageFormat::Png => encode_png(&rgb, SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT),
                _ => rgb,
            },
        })
    }

    /// Raw pixels for an observation: `RawGray` if `grayscale`, else `RawRgb`, shrunk by
    /// averaging `downscale`x`downscale` blocks (0 or 1 = full size; at most 256).
    pub fn observe_screen(&self, grayscale: bool, downscale: u32) -> Result<Image, Error> {
        if downscale > SCREENSHOT_WIDTH {
            return Err(Error::InvalidArgument(format!(
                "downscale {downscale} is larger than the {SCREENSHOT_WIDTH}-pixel screen width"
            )));
        }
        let (width, height, data) = scaled_pixels(self.framebuffer(), grayscale, downscale);
        Ok(Image {
            width,
            height,
            format: if grayscale {
                ImageFormat::RawGray
            } else {
                ImageFormat::RawRgb
            },
            data,
        })
    }
}
