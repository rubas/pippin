//! How a stream's Y'CbCr samples map to R'G'B': the range and the matrix
//! that the HEVC stream signals in its VUI, as FFmpeg reports them on the
//! decoded frame. The renderer's shader and the snapshots share the one
//! conversion that [`Colour::conversion`] gives.

use super::ffi::{AVColorRange, AVColorSpace};

/// The range of the samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    /// Y and Cb/Cr use 0 to 255.
    #[default]
    Full,
    /// Y uses 16 to 235, Cb/Cr 16 to 240.
    Limited,
}

/// The matrix coefficients from R'G'B' to Y'CbCr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Matrix {
    #[default]
    Bt709,
    Bt601,
    Bt2020,
}

/// The range and the matrix of a stream. The default is full-range
/// BT.709, for a stream that signals neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Colour {
    pub range: Range,
    pub matrix: Matrix,
}

/// R'G'B' = `rows` · (sample / 255 − `offset`), for the samples Y, Cb, Cr.
/// The layout is the shader's uniform: each row and the offset is a
/// `vec4<f32>` with an unused last element.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Conversion {
    pub rows: [[f32; 4]; 3],
    pub offset: [f32; 4],
}

impl Colour {
    /// The colour that FFmpeg reports on a frame. FFmpeg reports limited
    /// range also for an HEVC stream without a video signal type, as H.265
    /// infers it then (hevcdec.c, `export_stream_params`). A stream with a
    /// colour description has a video signal type. So limited range counts
    /// only with a matrix; without one, the stream keeps full range. A
    /// matrix that we do not convert keeps BT.709.
    pub fn from_ffmpeg(range: AVColorRange::Type, space: AVColorSpace::Type) -> Self {
        let matrix = match space {
            AVColorSpace::AVCOL_SPC_BT470BG | AVColorSpace::AVCOL_SPC_SMPTE170M => Some(Matrix::Bt601),
            AVColorSpace::AVCOL_SPC_BT2020_NCL => Some(Matrix::Bt2020),
            AVColorSpace::AVCOL_SPC_UNSPECIFIED => None,
            _ => Some(Matrix::Bt709),
        };
        let range = match (range, matrix) {
            (AVColorRange::AVCOL_RANGE_MPEG, Some(_)) => Range::Limited,
            _ => Range::Full,
        };
        Colour { range, matrix: matrix.unwrap_or_default() }
    }

    pub fn conversion(self) -> Conversion {
        let (kr, kb) = match self.matrix {
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt2020 => (0.2627, 0.0593),
        };
        let kg = 1.0 - kr - kb;
        // Scales of Y and Cb/Cr to 0..1 and −0.5..0.5, and the offsets of
        // Y and Cb/Cr. Neutral chroma is 128 in both ranges.
        let (y_scale, c_scale, y_offset, c_offset) = match self.range {
            Range::Full => (1.0, 1.0, 0.0, 128.0 / 255.0),
            Range::Limited => (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0, 128.0 / 255.0),
        };
        Conversion {
            rows: [
                [y_scale, 0.0, 2.0 * (1.0 - kr) * c_scale, 0.0],
                [y_scale, -2.0 * kb * (1.0 - kb) / kg * c_scale, -2.0 * kr * (1.0 - kr) / kg * c_scale, 0.0],
                [y_scale, 2.0 * (1.0 - kb) * c_scale, 0.0, 0.0],
            ],
            offset: [y_offset, c_offset, c_offset, 0.0],
        }
    }
}

impl Conversion {
    /// The R'G'B' bytes of one sample, as the shader computes them.
    pub fn rgb(&self, yuv: [u8; 3]) -> [u8; 3] {
        let v: [f32; 3] = std::array::from_fn(|i| yuv[i] as f32 / 255.0 - self.offset[i]);
        self.rows.map(|r| ((r[0] * v[0] + r[1] * v[1] + r[2] * v[2]).clamp(0.0, 1.0) * 255.0).round() as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITED_709: Colour = Colour { range: Range::Limited, matrix: Matrix::Bt709 };

    #[test]
    fn limited_range_bt709_maps_16_to_black_and_235_to_white() {
        let c = LIMITED_709.conversion();
        assert_eq!(c.rgb([16, 128, 128]), [0, 0, 0]);
        assert_eq!(c.rgb([235, 128, 128]), [255, 255, 255]);
        assert_eq!(c.rgb([126, 128, 128]), [128, 128, 128]);
    }

    #[test]
    fn full_range_maps_0_to_black_and_255_to_white() {
        let c = Colour::default().conversion();
        assert_eq!(c.rgb([0, 128, 128]), [0, 0, 0]);
        assert_eq!(c.rgb([255, 128, 128]), [255, 255, 255]);
    }

    #[test]
    fn full_range_grey_stays_grey() {
        // The window grey of macOS, #ECECEC.
        assert_eq!(Colour::default().conversion().rgb([236, 128, 128]), [236, 236, 236]);
    }

    #[test]
    fn bt601_red_is_red_only_with_the_bt601_matrix() {
        // Limited-range BT.601 red: Y 81, Cb 90, Cr 240. The 8-bit samples
        // round, so red comes back as 254.
        let red = [81, 90, 240];
        assert_eq!(Colour { range: Range::Limited, matrix: Matrix::Bt601 }.conversion().rgb(red), [254, 0, 0]);
        assert_eq!(LIMITED_709.conversion().rgb(red), [255, 24, 0]);
    }

    #[test]
    fn bt2020_red_is_red() {
        // Limited-range BT.2020 red: Y 74, Cb 97, Cr 240, rounded as above.
        let c = Colour { range: Range::Limited, matrix: Matrix::Bt2020 }.conversion();
        assert_eq!(c.rgb([74, 97, 240]), [255, 0, 1]);
    }

    #[test]
    fn takes_the_range_and_matrix_that_ffmpeg_reports() {
        use AVColorRange::*;
        use AVColorSpace::*;
        let cases = [
            ((AVCOL_RANGE_MPEG, AVCOL_SPC_BT709), LIMITED_709),
            ((AVCOL_RANGE_JPEG, AVCOL_SPC_BT709), Colour::default()),
            ((AVCOL_RANGE_JPEG, AVCOL_SPC_BT470BG), Colour { range: Range::Full, matrix: Matrix::Bt601 }),
            ((AVCOL_RANGE_MPEG, AVCOL_SPC_SMPTE170M), Colour { range: Range::Limited, matrix: Matrix::Bt601 }),
            ((AVCOL_RANGE_MPEG, AVCOL_SPC_BT2020_NCL), Colour { range: Range::Limited, matrix: Matrix::Bt2020 }),
            ((AVCOL_RANGE_JPEG, AVCOL_SPC_UNSPECIFIED), Colour::default()),
        ];
        for ((range, space), want) in cases {
            assert_eq!(Colour::from_ffmpeg(range, space), want, "range {range}, space {space}");
        }
    }

    #[test]
    fn a_stream_that_signals_nothing_keeps_full_range_bt709() {
        // FFmpeg reports limited range for an HEVC stream without a video
        // signal type, and no matrix.
        let c = Colour::from_ffmpeg(AVColorRange::AVCOL_RANGE_MPEG, AVColorSpace::AVCOL_SPC_UNSPECIFIED);
        assert_eq!(c, Colour::default());
        let c = Colour::from_ffmpeg(AVColorRange::AVCOL_RANGE_UNSPECIFIED, AVColorSpace::AVCOL_SPC_UNSPECIFIED);
        assert_eq!(c, Colour::default());
    }

    #[test]
    fn a_matrix_we_do_not_convert_keeps_bt709_and_the_signalled_range() {
        let c = Colour::from_ffmpeg(AVColorRange::AVCOL_RANGE_MPEG, AVColorSpace::AVCOL_SPC_BT2020_CL);
        assert_eq!(c, LIMITED_709);
    }
}
