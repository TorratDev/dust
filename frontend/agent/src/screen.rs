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
        let rgb = framebuffer_rgb(self.framebuffer());
        Ok(Image {
            width: SCREENSHOT_WIDTH,
            height: SCREENSHOT_HEIGHT,
            format,
            data: match format {
                ImageFormat::RawRgb => rgb,
                ImageFormat::Png => encode_png(&rgb, SCREENSHOT_WIDTH, SCREENSHOT_HEIGHT),
            },
        })
    }
}
