//! Snapshots of the decoded canvas as RGB PNG files, for scripted tests.
//! The colours match the renderer's shader, with the stream's colour; the
//! chroma takes the nearest sample instead of a filtered one.

use std::path::Path;

use anyhow::{Context, Result};
use image::{Rgb, RgbImage};

use crate::video::colour::Conversion;
use crate::video::decoder::{Layout, Picture, PlaneRef};

/// The latest picture of each tile, kept to draw the whole canvas.
#[derive(Default)]
pub struct Canvas {
    /// Canvas size announced by the host; tiles stack into this.
    size: Option<(u32, u32)>,
    tiles: Vec<Option<Picture>>,
}

impl Canvas {
    /// A new canvas size makes the pictures of the old one stale.
    pub fn resize(&mut self, width: u32, height: u32) {
        if self.size != Some((width, height)) {
            self.size = Some((width, height));
            self.tiles.clear();
        }
    }

    /// Keeps the newest picture of each tile. All tiles have one size: a
    /// picture of another size starts a new layout and drops the tiles of
    /// the old one, so a snapshot never mixes the two.
    pub fn put(&mut self, pictures: Vec<(usize, Picture)>) {
        for (tile, pic) in pictures.into_iter().filter(|(_, p)| p.width > 0 && p.height > 0) {
            let size = |p: &Picture| (p.width, p.height);
            if self.tiles.iter().flatten().any(|held| size(held) != size(&pic)) {
                self.tiles.clear();
            }
            if self.tiles.len() <= tile {
                self.tiles.resize_with(tile + 1, || None);
            }
            self.tiles[tile] = Some(pic);
        }
    }

    /// The canvas in RGB at full resolution, once every tile has a picture.
    pub fn image(&self) -> Option<RgbImage> {
        let (_, height) = self.size?;
        let first = self.tiles.first()?.as_ref()?;
        let tiles = self.tiles.get(..height.div_ceil(first.height) as usize)?;
        if tiles.iter().any(Option::is_none) {
            return None;
        }
        let mut img = RgbImage::new(first.width, height);
        for (i, pic) in tiles.iter().flatten().enumerate() {
            let planes: Vec<_> = (0..pic.layout.planes()).map(|p| pic.plane(p)).collect();
            draw_tile(&mut img, i as u32 * pic.height, pic.layout, &pic.colour.conversion(), &planes);
        }
        Some(img)
    }
}

/// Converts one tile's planes into `img` from row `y0` down, as the
/// renderer stacks tiles. What falls outside `img` is left out.
fn draw_tile(img: &mut RgbImage, y0: u32, layout: Layout, conversion: &Conversion, planes: &[PlaneRef]) {
    let luma = planes[0];
    // Plane, bytes per texel and byte in the texel of Y, U and V.
    let components: [_; 3] = std::array::from_fn(|c| {
        let p = c.min(layout.planes() - 1);
        (planes[p], layout.texel_bytes(p), layout.channels()[c])
    });
    let width = luma.width.min(img.width());
    let height = luma.height.min(img.height().saturating_sub(y0));
    for y in 0..height {
        for x in 0..width {
            let yuv = components.map(|(p, texel, ch)| {
                let (px, py) = (x * p.width / luma.width, y * p.height / luma.height);
                p.data[(py * p.stride + px * texel + ch) as usize]
            });
            img.put_pixel(x, y0 + y, Rgb(conversion.rgb(yuv)));
        }
    }
}

/// Writes `img` to `<path>.tmp` and renames it to `path`, so a file at
/// `path` is always a whole PNG.
pub fn write_png(img: &RgbImage, path: &Path) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    img.save_with_format(&tmp, image::ImageFormat::Png).context("writing PNG")?;
    std::fs::rename(&tmp, path).context("renaming PNG into place")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::colour::{Colour, Matrix, Range};

    const RED: [u8; 3] = [54, 98, 255];
    const BLUE: [u8; 3] = [18, 255, 116];
    /// RED and BLUE in RGB: 8-bit full-range samples round pure red and
    /// blue to 254.
    const RED_RGB: [u8; 3] = [254, 0, 0];
    const BLUE_RGB: [u8; 3] = [0, 0, 254];

    fn plane(data: &[u8], stride: u32, width: u32, height: u32) -> PlaneRef<'_> {
        PlaneRef { data, stride, width, height }
    }

    #[test]
    fn stacks_two_planar_tiles_into_one_png() {
        // Two 4×2 tiles with 4:2:0 chroma and padded rows: red on top of
        // blue, in a canvas 3 rows high, so the blue tile is cut to 1 row.
        let tile = |c: [u8; 3]| -> [Vec<u8>; 3] {
            let luma = [c[0], c[0], c[0], c[0], 0, 0].repeat(2);
            [luma, vec![c[1], c[1], 7], vec![c[2], c[2], 7]]
        };
        let mut img = RgbImage::new(4, 3);
        for (i, c) in [RED, BLUE].into_iter().enumerate() {
            let [y, u, v] = tile(c);
            let planes = [plane(&y, 6, 4, 2), plane(&u, 3, 2, 1), plane(&v, 3, 2, 1)];
            draw_tile(&mut img, i as u32 * 2, Layout::Planar, &Colour::default().conversion(), &planes);
        }
        let path = std::env::temp_dir().join(format!("pippin-snapshot-test-{}.png", std::process::id()));
        write_png(&img, &path).unwrap();
        let back = image::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(back.color(), image::ColorType::Rgb8);
        let back = back.into_rgb8();
        assert_eq!(back.dimensions(), (4, 3));
        for x in 0..4 {
            assert_eq!(back.get_pixel(x, 0).0, RED_RGB);
            assert_eq!(back.get_pixel(x, 1).0, RED_RGB);
            assert_eq!(back.get_pixel(x, 2).0, BLUE_RGB);
        }
    }

    #[test]
    fn a_snapshot_never_mixes_tiles_of_two_sizes() {
        let mut canvas = Canvas::default();
        canvas.resize(4, 4);
        // A late tile 0 of the old 4×4 layout, then tile 1 of the new 4×2 one.
        canvas.put(vec![(0, Picture::filled(4, 4, BLUE))]);
        canvas.put(vec![(1, Picture::filled(4, 2, RED))]);
        assert!(canvas.image().is_none(), "tile 0 of the new layout is still missing");
        canvas.put(vec![(0, Picture::filled(4, 2, RED))]);
        let img = canvas.image().unwrap();
        assert_eq!(img.dimensions(), (4, 4));
        assert!(img.pixels().all(|p| p.0 == RED_RGB));
    }

    #[test]
    fn a_snapshot_converts_with_the_colour_of_its_picture() {
        // Limited-range white over limited-range black, in two tiles.
        let limited = Colour { range: Range::Limited, matrix: Matrix::Bt709 };
        let mut canvas = Canvas::default();
        canvas.resize(2, 4);
        for (tile, y) in [(0, 235), (1, 16)] {
            let mut pic = Picture::filled(2, 2, [y, 128, 128]);
            pic.colour = limited;
            canvas.put(vec![(tile, pic)]);
        }
        let img = canvas.image().unwrap();
        assert_eq!([img.get_pixel(0, 0).0, img.get_pixel(1, 3).0], [[255; 3], [0; 3]]);
    }

    #[test]
    fn a_failed_write_leaves_no_file() {
        let path = std::env::temp_dir().join(format!("pippin-no-such-dir-{}", std::process::id())).join("a.png");
        assert!(write_png(&RgbImage::new(2, 2), &path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn reads_semi_planar_and_vuyx_channels() {
        // NV12: one 2×2 tile, the chroma plane holds U then V.
        let (y, uv) = ([RED[0]; 4], [RED[1], RED[2]]);
        let mut img = RgbImage::new(2, 2);
        draw_tile(&mut img, 0, Layout::SemiPlanar, &Colour::default().conversion(), &[plane(&y, 2, 2, 2), plane(&uv, 2, 1, 1)]);
        assert!(img.pixels().all(|p| p.0 == RED_RGB));
        // VUYX: one plane of V, U, Y, X bytes.
        let vuyx = [BLUE[2], BLUE[1], BLUE[0], 0xff].repeat(4);
        draw_tile(&mut img, 0, Layout::Vuyx, &Colour::default().conversion(), &[plane(&vuyx, 8, 2, 2)]);
        assert!(img.pixels().all(|p| p.0 == BLUE_RGB));
    }
}
