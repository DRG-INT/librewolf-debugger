//! Decoding and resampling, shared by both backends.
//!
//! Everything here works on 8-bit luma. Engines differ legitimately in
//! subpixel antialiasing, so comparing colour channels across engines reports
//! differences that are not defects. Luma keeps the comparison about layout
//! and glyph placement, which is what parity means.

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Luma {
    pub width: usize,
    pub height: usize,
    /// One byte per pixel, row-major.
    pub data: Vec<u8>,
}

impl Luma {
    pub fn new(width: usize, height: usize, data: Vec<u8>) -> Result<Self> {
        if data.len() != width * height {
            bail!(
                "{} bytes for a {width}x{height} image, expected {}",
                data.len(),
                width * height
            );
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.data[y * self.width + x]
    }

    /// Box-filter resample. Used to bring two renders to a common size and to
    /// produce the 32x32 input for the perceptual hash.
    ///
    /// A box filter rather than nearest-neighbour on purpose: nearest drops
    /// whole rows of text at the scales a hash uses, and two pages that
    /// differ by one line can hash identically.
    pub fn resize(&self, w: usize, h: usize) -> Result<Luma> {
        if w == 0 || h == 0 {
            bail!("cannot resize to {w}x{h}");
        }
        if w == self.width && h == self.height {
            return Ok(self.clone());
        }
        let mut out = vec![0u8; w * h];
        for oy in 0..h {
            let y0 = oy * self.height / h;
            let y1 = ((oy + 1) * self.height)
                .div_ceil(h)
                .max(y0 + 1)
                .min(self.height);
            for ox in 0..w {
                let x0 = ox * self.width / w;
                let x1 = ((ox + 1) * self.width)
                    .div_ceil(w)
                    .max(x0 + 1)
                    .min(self.width);
                let mut sum = 0u32;
                let mut n = 0u32;
                for y in y0..y1 {
                    for x in x0..x1 {
                        sum += self.at(x, y) as u32;
                        n += 1;
                    }
                }
                out[oy * w + ox] = (sum / n.max(1)) as u8;
            }
        }
        Luma::new(w, h, out)
    }

    /// Crop both images to their common top-left region.
    ///
    /// Two browsers rarely agree on viewport height to the pixel. Padding the
    /// shorter one with black would invent a large uniform difference and
    /// drive SSIM down for a reason that has nothing to do with rendering;
    /// cropping compares only what both actually drew, and the discarded
    /// extent is reported separately.
    pub fn common_crop(a: &Luma, b: &Luma) -> Result<(Luma, Luma)> {
        let w = a.width.min(b.width);
        let h = a.height.min(b.height);
        if w == 0 || h == 0 {
            bail!(
                "no overlap between {}x{} and {}x{}",
                a.width,
                a.height,
                b.width,
                b.height
            );
        }
        Ok((a.crop(w, h)?, b.crop(w, h)?))
    }

    pub fn crop(&self, w: usize, h: usize) -> Result<Luma> {
        if w > self.width || h > self.height {
            bail!("cannot crop {}x{} to {w}x{h}", self.width, self.height);
        }
        if w == self.width && h == self.height {
            return Ok(self.clone());
        }
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h {
            out.extend_from_slice(&self.data[y * self.width..y * self.width + w]);
        }
        Luma::new(w, h, out)
    }

    /// For the GPU path: f32 per pixel, which is what a storage buffer wants.
    pub fn to_f32(&self) -> Vec<f32> {
        self.data.iter().map(|&v| v as f32).collect()
    }
}

/// Decode a PNG into luma. Screenshots arrive as PNG from every transport.
pub fn decode_png(bytes: &[u8]) -> Result<Luma> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder.read_info().context("reading PNG header")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("decoding PNG")?;
    let (w, h) = (info.width as usize, info.height as usize);
    let px = &buf[..info.buffer_size()];

    // Rec. 709 luma. The integer form keeps both backends bit-identical,
    // which matters because the GPU/CPU agreement test is a real check and
    // not a formality.
    let luma = |r: u8, g: u8, b: u8| -> u8 {
        ((2126 * r as u32 + 7152 * g as u32 + 722 * b as u32) / 10000) as u8
    };

    let data = match info.color_type {
        png::ColorType::Grayscale => px.to_vec(),
        png::ColorType::GrayscaleAlpha => px.chunks(2).map(|c| c[0]).collect(),
        png::ColorType::Rgb => px.chunks(3).map(|c| luma(c[0], c[1], c[2])).collect(),
        png::ColorType::Rgba => px.chunks(4).map(|c| luma(c[0], c[1], c[2])).collect(),
        png::ColorType::Indexed => {
            bail!("indexed PNG is not expected from a browser screenshot")
        }
    };
    Luma::new(w, h, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(w: usize, h: usize) -> Luma {
        let data = (0..w * h).map(|i| (i % 256) as u8).collect();
        Luma::new(w, h, data).unwrap()
    }

    #[test]
    fn dimension_mismatch_is_rejected_at_construction() {
        assert!(Luma::new(4, 4, vec![0; 15]).is_err());
        assert!(Luma::new(4, 4, vec![0; 16]).is_ok());
    }

    #[test]
    fn resizing_to_the_same_size_is_the_identity() {
        let a = ramp(9, 7);
        assert_eq!(a.resize(9, 7).unwrap(), a);
    }

    #[test]
    fn a_downscale_averages_rather_than_sampling() {
        // Two rows: all 0 and all 200. Nearest-neighbour would return one of
        // them; a box filter returns the mean.
        let a = Luma::new(2, 2, vec![0, 0, 200, 200]).unwrap();
        let s = a.resize(1, 1).unwrap();
        assert_eq!(s.data, vec![100]);
    }

    #[test]
    fn a_thin_line_survives_the_downscale_used_by_the_hash() {
        // One bright row in an otherwise black 32-row image. Nearest
        // neighbour can drop it entirely; the box filter must not.
        let (w, h) = (32, 32);
        let mut data = vec![0u8; w * h];
        for x in 0..w {
            data[7 * w + x] = 255;
        }
        let a = Luma::new(w, h, data).unwrap();
        let s = a.resize(8, 8).unwrap();
        assert!(
            s.data.iter().any(|&v| v > 0),
            "the line vanished, so two pages differing by a line would hash alike"
        );
    }

    #[test]
    fn upscaling_is_supported_without_dividing_by_zero() {
        let a = Luma::new(2, 2, vec![10, 20, 30, 40]).unwrap();
        let s = a.resize(4, 4).unwrap();
        assert_eq!(s.width, 4);
        assert!(s.data.iter().all(|&v| (10..=40).contains(&v)));
    }

    #[test]
    fn common_crop_takes_the_overlap_rather_than_padding() {
        let a = ramp(100, 80);
        let b = ramp(100, 60);
        let (ca, cb) = Luma::common_crop(&a, &b).unwrap();
        assert_eq!((ca.width, ca.height), (100, 60));
        assert_eq!((cb.width, cb.height), (100, 60));
        // Padding would have introduced 100x20 of invented difference.
        assert_eq!(ca.data.len(), 100 * 60);
    }

    #[test]
    fn no_overlap_is_an_error_not_an_empty_comparison() {
        let a = ramp(10, 10);
        let b = Luma::new(0, 0, vec![]).unwrap();
        assert!(Luma::common_crop(&a, &b).is_err());
    }

    #[test]
    fn png_round_trips_through_the_decoder() {
        let (w, h) = (16usize, 9usize);
        let mut rgba = Vec::new();
        for i in 0..w * h {
            let v = (i * 3 % 256) as u8;
            rgba.extend_from_slice(&[v, v, v, 255]);
        }
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().unwrap();
            wr.write_image_data(&rgba).unwrap();
        }
        let l = decode_png(&out).unwrap();
        assert_eq!((l.width, l.height), (w, h));
        // Grey in, grey out: luma of (v,v,v) is v.
        assert_eq!(l.at(0, 0), 0);
        assert_eq!(l.at(1, 0), 3);
    }

    #[test]
    fn a_truncated_png_is_an_error_not_a_blank_image() {
        let bytes = b"\x89PNG\r\n\x1a\n";
        assert!(decode_png(bytes).is_err());
    }
}
