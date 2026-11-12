use crate::data::{Image, ImageType, PixelBuffer};
use anyhow::{bail, Result};
use fitskit::HeaderValue;
use crate::keywords::EXPTIME;

pub struct ChannelInput {
    pub image: Image,

    // contribution of the image to different channels, as absolute gains
    // each value have range 0..1, they DON'T have to sum up to 1.0
    // (0.5 halves the image's contribution; contributions summing past 1.0 clip at white)
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub lum: f32,
}

fn new_channel_input(image: Image) -> Result<ChannelInput> {
    match image.image_type {
        ImageType::Grayscale => Ok(ChannelInput{
            image,
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            lum: 0.0,
        }),
        _ => bail!("Only greyscale images are supported for compositing input")
    }
}

fn linearize(c: f32) -> f32 {
    if c < 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn normalize(c: u16) -> f32 {
    c as f32 / (u16::MAX as f32)
}

/// D65 reference white, as produced by the sRGB matrix below for RGB (1, 1, 1), so that grays
/// have exactly zero chroma.
const WHITE_REF: [f32; 3] = [0.950_456, 1.0, 1.089_058];

fn lab_f(t: f32) -> f32 {
    const THRESHOLD: f32 = (6.0 / 29.0) * (6.0 / 29.0) * (6.0 / 29.0);
    if t > THRESHOLD {
        t.cbrt()
    } else {
        t / 3.0 * (29.0 / 6.0) * (29.0 / 6.0) + 4.0 / 29.0
    }
}

// Converts 16bit 3-colour RGB value to CIE Lab
// Result is an array of 3 values: L, a, and b
// `L` is scaled 0..1, `a` and `b` are roughly ±1.
pub fn rgb_to_lab(px: [u16;3]) -> [f32; 3] {
    let r = linearize(normalize(px[0]));
    let g = linearize(normalize(px[1]));
    let b = linearize(normalize(px[2]));
    let x = 0.412_390_8 * r + 0.357_584_34 * g + 0.180_480_8 * b;
    let y = 0.212_639 * r + 0.715_168_7 * g + 0.072_192_32 * b;
    let z = 0.019_330_819 * r + 0.119_194_78 * g + 0.950_532_15 * b;
    let fy = lab_f(y / WHITE_REF[1]);
    [
        1.16 * fy - 0.16,
        5.0 * (lab_f(x / WHITE_REF[0]) - fy),
        2.0 * (fy - lab_f(z / WHITE_REF[2])),
    ]
}

fn delinearize(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

fn lab_f_inv(t: f32) -> f32 {
    const DELTA: f32 = 6.0 / 29.0;
    if t > DELTA {
        t * t * t
    } else {
        3.0 * DELTA * DELTA * (t - 4.0 / 29.0)
    }
}

/// Inverse of [`rgb_to_lab`] (using the exact inverse of its RGB→XYZ matrix): scaled L*a*b* (see [`rgb_to_lab`]) to 16-bit sRGB. Out-of-gamut colours are
/// clamped per channel.
pub fn lab_to_rgb(lab: [f32; 3]) -> [u16; 3] {
    let fy = (lab[0] + 0.16) / 1.16;
    let fx = fy + lab[1] / 5.0;
    let fz = fy - lab[2] / 2.0;
    let x = WHITE_REF[0] * lab_f_inv(fx);
    let y = WHITE_REF[1] * lab_f_inv(fy);
    let z = WHITE_REF[2] * lab_f_inv(fz);
    let r = 3.240_97 * x - 1.537_383 * y - 0.498_610_8 * z;
    let g = -0.969_243_6 * x + 1.875_967_4 * y + 0.041_555_05 * z;
    let b = 0.055_630_07 * x - 0.203_976_95 * y + 1.056_971_5 * z;
    [r, g, b].map(|c| (delinearize(c).clamp(0.0, 1.0) * u16::MAX as f32).round() as u16)
}

pub struct RgbComposer<'a> {
    /// 3 or 4 channels - first 3 is colour data,
    /// Last channel is always luminance
    pub channels: Vec<&'a ChannelInput>,
    pub add_times: bool
}

pub fn new_rgb_composer(channels: Vec<&ChannelInput>) -> Result<RgbComposer<'_>> {
    if channels.len() < 3 || channels.len() > 4 {
        bail!("RGB composition requires three or four input channels")
    }
    let leading_channel = channels.first().unwrap();
    if !channels.iter().skip(1).all(|c|
        c.image.width == leading_channel.image.width && c.image.height == leading_channel.image.height
    ) {
        bail!("Images must be of the same size")
    }

    Ok(RgbComposer{
        channels,
        add_times: true
    })
}

fn sum_exposure(images: &[&Image]) -> f32 {
    let result: f64 = images.iter()
        .map(|i|i.header.get_float(EXPTIME).unwrap_or(0.0))
        .sum();
    result as f32
}

/// Weighted sum of the three input samples, clamped to the 16-bit range. The weights are
/// absolute gains, not shares: 0.5 halves an input and weights summing past 1.0 clip.
fn combine(px: [u16;3], r: f32, g: f32, b: f32) -> u16 {
    let result = (px[0] as f32) * r + (px[1] as f32) * g + (px[2] as f32) * b;
    result.clamp(0.0, u16::MAX as f32).round() as u16
}
impl RgbComposer<'_> {

    pub fn compose(&self) -> Result<Image> {
        let width = self.channels[0].image.width;
        let height = self.channels[0].image.height;
        let pix1 = self.channels[0].image.pixels.as_u16();
        let pix2 = self.channels[1].image.pixels.as_u16();
        let pix3 = self.channels[2].image.pixels.as_u16();
        let pix_l = match self.channels.len() > 3 {
            true => Some(self.channels[3].image.pixels.as_f32()),
            false => None
        };
        let mut result_rgb:Vec<u16> = Vec::with_capacity(width * height*3);
        for y in 0..height {
            for x in 0..width {
                let offset = y * width + x;
                let px = [pix1[offset], pix2[offset], pix3[offset]];
                let r = combine(px,
                                self.channels[0].red, self.channels[1].red, self.channels[2].red
                );
                let g = combine(px,
                                self.channels[0].green, self.channels[1].green, self.channels[2].green
                );
                let b = combine(px,
                                self.channels[0].blue, self.channels[1].blue, self.channels[2].blue
                );
                let mut rgb = [r,g,b];
                if let Some(l_pix) = &pix_l && self.channels[3].lum > 0.0 {
                    let a = self.channels[3].lum;
                    let l = l_pix[offset];
                    let mut lab = rgb_to_lab(rgb);
                    lab[0] = l * a + lab[0] * (1.0-a);
                    rgb = lab_to_rgb(lab);
                }
                result_rgb.push(rgb[0]);
                result_rgb.push( rgb[1]);
                result_rgb.push(rgb[2]);
            }
        }

        let mut rgb_image = Image::new(
            ImageType::RGB, self.channels[0].image.header.clone(), width, height,
        PixelBuffer::U16(result_rgb));
        if self.add_times {
            rgb_image.header.set(
                EXPTIME,
                HeaderValue::Float(sum_exposure(&self.channels.iter().map(|c| &c.image).collect::<Vec<_>>()) as f64),
                None);
        }

        Ok(rgb_image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lab` returns CIE L*a*b* divided by 100 (L in 0..=1), so references below are scaled.
    fn assert_lab(px: [u16; 3], expected: [f32; 3], tol: f32) {
        let got = rgb_to_lab(px);
        for i in 0..3 {
            let want = expected[i] / 100.0;
            assert!(
                (got[i] - want).abs() < tol,
                "{px:?} channel {i}: got {}, want {want}",
                got[i]
            );
        }
    }

    const MAX: u16 = u16::MAX;

    #[test]
    fn black_and_white() {
        assert_lab([0, 0, 0], [0.0, 0.0, 0.0], 1e-5);
        assert_lab([MAX, MAX, MAX], [100.0, 0.0, 0.0], 1e-4);
    }

    #[test]
    fn grays_have_no_chroma() {
        for v in [1u16, 100, 5000, 32768, 60000] {
            let [_, a, b] = rgb_to_lab([v, v, v]);
            assert!(a.abs() < 1e-3 && b.abs() < 1e-3, "v={v}: a={a} b={b}");
        }
    }

    #[test]
    fn lightness_is_monotonic_in_gray() {
        let mut prev = -1.0;
        for v in (0..=MAX).step_by(257) {
            let l = rgb_to_lab([v, v, v])[0];
            assert!(l > prev, "L* not increasing at {v}");
            prev = l;
        }
    }

    #[test]
    fn srgb_primaries_match_reference() {
        // Reference values for sRGB primaries under D65 (e.g. Lindbloom / colour-science).
        assert_lab([MAX, 0, 0], [53.2408, 80.0925, 67.2032], 2e-3);
        assert_lab([0, MAX, 0], [87.7347, -86.1827, 83.1793], 2e-3);
        assert_lab([0, 0, MAX], [32.2970, 79.1875, -107.8602], 2e-3);
    }

    #[test]
    fn mid_gray_matches_reference() {
        // 8-bit 128 (== 128 * 257 in 16 bit) has L* = 53.585.
        assert_lab([128 * 257; 3], [53.585, 0.0, 0.0], 1e-3);
    }

    #[test]
    fn dark_linear_segment() {
        // Below the sRGB (0.04045) and Lab (216/24389) knees both curves are linear:
        // L* = 903.3 * Y, with Y = c / 12.92 for gray.
        let v = 100u16;
        let y = (v as f32 / MAX as f32) / 12.92;
        assert_lab([v; 3], [903.296 * y, 0.0, 0.0], 1e-3);
    }

    #[test]
    fn linearize_is_continuous_at_knee() {
        let below = linearize(0.04045 - 1e-6);
        let above = linearize(0.04045 + 1e-6);
        assert!((above - below).abs() < 1e-5);
    }

    #[test]
    fn lab_f_is_continuous_at_knee() {
        let t = (6.0f32 / 29.0).powi(3);
        assert!((lab_f(t - 1e-7) - lab_f(t + 1e-7)).abs() < 1e-4);
    }

    #[test]
    fn lab_to_rgb_known_values() {
        assert_eq!(lab_to_rgb([0.0, 0.0, 0.0]), [0, 0, 0]);
        assert_eq!(lab_to_rgb([1.0, 0.0, 0.0]), [MAX; 3]);
        let red = lab_to_rgb([0.532408, 0.800925, 0.672032]);
        assert!(red[0] >= MAX - 40 && red[1] <= 40 && red[2] <= 40, "{red:?}");
    }

    #[test]
    fn lab_to_rgb_clamps_out_of_gamut() {
        let [r, g, b] = lab_to_rgb([0.5, 3.0, -3.0]);
        assert!(r == MAX || g == 0 || b == MAX || r == 0 || g == MAX || b == 0);
        assert_eq!(lab_to_rgb([2.0, 0.0, 0.0]), [MAX; 3]);
        assert_eq!(lab_to_rgb([-1.0, 0.0, 0.0]), [0, 0, 0]);
    }

    #[test]
    fn rgb_lab_round_trip() {
        let mut worst = 0i32;
        let levels = [0u16, 1, 2, 10, 200, 1000, 4000, 20000, 32768, 50000, 65534, MAX];
        for &r in &levels {
            for &g in &levels {
                for &b in &levels {
                    let back = lab_to_rgb(rgb_to_lab([r, g, b]));
                    for (a, e) in back.iter().zip([r, g, b]) {
                        worst = worst.max((*a as i32 - e as i32).abs());
                    }
                }
            }
        }
        assert!(worst <= 2, "worst round-trip error {worst} counts of 65535");
    }

    // ---- RgbComposer::compose ----

    use crate::fits_file::load_fits;
    use crate::test_support::test_data;

    fn input(image: Image, red: f32, green: f32, blue: f32, lum: f32) -> ChannelInput {
        ChannelInput { image, red, green, blue, lum }
    }

    /// The three planes of the bundled RGB fixture as separate mono images.
    fn real_planes() -> [Image; 3] {
        load_fits(&test_data("rgb.fits")).unwrap().split_channels().unwrap()
    }

    fn mono_like(template: &Image, pixels: PixelBuffer) -> Image {
        Image::new(ImageType::Grayscale, template.header.clone(), template.width, template.height, pixels)
    }

    fn compose(channels: &[&ChannelInput]) -> Image {
        new_rgb_composer(channels.to_vec()).unwrap().compose().unwrap()
    }

    fn u16s(img: &Image) -> &[u16] {
        match &img.pixels {
            PixelBuffer::U16(v) => v,
            PixelBuffer::F32(_) => panic!("expected a 16-bit result"),
        }
    }

    #[test]
    fn compose_identity_weights_reassemble_the_original_rgb() {
        let original = load_fits(&test_data("rgb.fits")).unwrap();
        let [r, g, b] = real_planes();
        let (cr, cg, cb) = (
            input(r, 1.0, 0.0, 0.0, 0.0),
            input(g, 0.0, 1.0, 0.0, 0.0),
            input(b, 0.0, 0.0, 1.0, 0.0),
        );
        let out = compose(&[&cr, &cg, &cb]);

        assert_eq!(out.image_type, ImageType::RGB);
        assert_eq!((out.width, out.height), (original.width, original.height));
        assert_eq!(u16s(&out).len(), original.width * original.height * 3);
        // Both are interleaved R,G,B,R,G,B,...
        assert_eq!(u16s(&out), original.pixels.as_u16().as_slice());
    }

    #[test]
    fn compose_output_is_interleaved() {
        let [t, _, _] = real_planes();
        let (w, h) = (t.width, t.height);
        let n = w * h;
        let plane = |v: u16| mono_like(&t, PixelBuffer::U16(vec![v; n]));
        let (c1, c2, c3) = (
            input(plane(1000), 1.0, 0.0, 0.0, 0.0),
            input(plane(2000), 0.0, 1.0, 0.0, 0.0),
            input(plane(3000), 0.0, 0.0, 1.0, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3]);
        for px in u16s(&out).chunks_exact(3) {
            assert_eq!(px, [1000, 2000, 3000]);
        }
    }

    #[test]
    fn compose_pixel_order_follows_row_major_offsets() {
        let [t, _, _] = real_planes();
        let n = t.width * t.height;
        let ramp: Vec<u16> = (0..n).map(|i| (i % 60000) as u16).collect();
        let c1 = input(mono_like(&t, PixelBuffer::U16(ramp.clone())), 1.0, 0.0, 0.0, 0.0);
        let c2 = input(mono_like(&t, PixelBuffer::U16(vec![0; n])), 0.0, 1.0, 0.0, 0.0);
        let c3 = input(mono_like(&t, PixelBuffer::U16(vec![0; n])), 0.0, 0.0, 1.0, 0.0);
        let out = compose(&[&c1, &c2, &c3]);
        for (i, px) in u16s(&out).chunks_exact(3).enumerate() {
            assert_eq!(px, [ramp[i], 0, 0], "pixel {i}");
        }
    }

    #[test]
    fn compose_mixes_channels_by_weight() {
        let [r, g, b] = real_planes();
        let (pr, pg, pb) = (r.pixels.as_u16(), g.pixels.as_u16(), b.pixels.as_u16());
        // Output red = 0.75*ch0 + 0.25*ch1; output green = 0.5*ch2 (dimmed, not renormalised);
        // output blue = a third of each.
        let third = 1.0 / 3.0;
        let (c1, c2, c3) = (
            input(r, 0.75, 0.0, third, 0.0),
            input(g, 0.25, 0.0, third, 0.0),
            input(b, 0.0, 0.5, third, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3]);
        for (i, px) in u16s(&out).chunks_exact(3).enumerate() {
            let er = 0.75 * pr[i] as f64 + 0.25 * pg[i] as f64;
            let eg = 0.5 * pb[i] as f64;
            let eb = (pr[i] as f64 + pg[i] as f64 + pb[i] as f64) / 3.0;
            for (got, want) in px.iter().zip([er, eg, eb]) {
                assert!((*got as f64 - want).abs() <= 1.0, "pixel {i}: got {got}, want {want}");
            }
        }
    }

    #[test]
    fn compose_zero_weights_for_a_colour_give_black() {
        let [r, g, b] = real_planes();
        let (c1, c2, c3) = (
            input(r, 1.0, 0.0, 0.0, 0.0),
            input(g, 0.0, 0.0, 0.0, 0.0),
            input(b, 0.0, 0.0, 0.0, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3]);
        assert!(u16s(&out).chunks_exact(3).all(|px| px[1] == 0 && px[2] == 0));
    }

    fn colour_inputs(lum: f32) -> (ChannelInput, ChannelInput, ChannelInput, ChannelInput) {
        let [r, g, b] = real_planes();
        let l = mono_like(&r, PixelBuffer::F32(vec![0.5; r.width * r.height]));
        (
            input(r, 1.0, 0.0, 0.0, 0.0),
            input(g, 0.0, 1.0, 0.0, 0.0),
            input(b, 0.0, 0.0, 1.0, 0.0),
            input(l, 0.0, 0.0, 0.0, lum),
        )
    }

    #[test]
    fn compose_zero_luminance_weight_leaves_colour_unchanged() {
        let (r, g, b, l) = colour_inputs(0.0);
        let without = compose(&[&r, &g, &b]);
        let with = compose(&[&r, &g, &b, &l]);
        // The Lab round trip is skipped entirely, so the result is bit-identical.
        assert_eq!(u16s(&without), u16s(&with));
    }

    #[test]
    fn compose_full_luminance_weight_replaces_lightness_and_keeps_hue() {
        let (r, g, b, l) = colour_inputs(1.0);
        let without = compose(&[&r, &g, &b]);
        let with = compose(&[&r, &g, &b, &l]);
        let mut checked = 0;
        for (a, b) in u16s(&without).chunks_exact(3).zip(u16s(&with).chunks_exact(3)) {
            let (la, lb) = (rgb_to_lab([a[0], a[1], a[2]]), rgb_to_lab([b[0], b[1], b[2]]));
            // Skip pixels whose new colour is clamped at the gamut edge or is black/white.
            if b.iter().any(|c| !(64..=MAX - 64).contains(c)) {
                continue;
            }
            assert!((lb[0] - 0.5).abs() < 2e-3, "L = {}", lb[0]);
            // Chroma direction is preserved (a/b unchanged by a lightness swap).
            assert!((la[1] - lb[1]).abs() < 2e-2 && (la[2] - lb[2]).abs() < 2e-2);
            checked += 1;
        }
        assert!(checked > 0, "no pixel was within gamut; test is vacuous");
    }

    #[test]
    fn compose_partial_luminance_weight_blends_lightness() {
        let (r, g, b, l) = colour_inputs(0.5);
        let without = compose(&[&r, &g, &b]);
        let with = compose(&[&r, &g, &b, &l]);
        let mut checked = 0;
        for (a, b) in u16s(&without).chunks_exact(3).zip(u16s(&with).chunks_exact(3)) {
            if b.iter().any(|c| !(64..=MAX - 64).contains(c)) {
                continue;
            }
            let want = 0.5 * 0.5 + 0.5 * rgb_to_lab([a[0], a[1], a[2]])[0];
            let got = rgb_to_lab([b[0], b[1], b[2]])[0];
            assert!((got - want).abs() < 2e-3, "got {got}, want {want}");
            checked += 1;
        }
        assert!(checked > 0);
    }

    #[test]
    fn compose_sums_exposure_unless_disabled() {
        let [mut r, mut g, mut b] = real_planes();
        for (img, t) in [(&mut r, 10.0), (&mut g, 20.0), (&mut b, 30.0)] {
            img.header.set(EXPTIME, HeaderValue::Float(t), None);
        }
        let (c1, c2, c3) = (
            input(r, 1.0, 0.0, 0.0, 0.0),
            input(g, 0.0, 1.0, 0.0, 0.0),
            input(b, 0.0, 0.0, 1.0, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3]);
        assert_eq!(out.header.get_float(EXPTIME), Some(60.0));

        let mut composer = new_rgb_composer(vec![&c1, &c2, &c3]).unwrap();
        composer.add_times = false;
        let out = composer.compose().unwrap();
        assert_eq!(out.header.get_float(EXPTIME), Some(10.0)); // first channel's header, untouched
    }

    #[test]
    fn new_rgb_composer_rejects_bad_input() {
        let [r, g, b] = real_planes();
        let small = mono_like(&r, PixelBuffer::U16(vec![0; 1]));
        let small = Image { width: 1, height: 1, ..small };
        let (c1, c2, c3) = (
            input(r, 1.0, 0.0, 0.0, 0.0),
            input(g, 0.0, 1.0, 0.0, 0.0),
            input(b, 0.0, 0.0, 1.0, 0.0),
        );
        assert!(new_rgb_composer(vec![&c1, &c2]).is_err());
        let c4 = input(small, 0.0, 0.0, 0.0, 1.0);
        assert!(new_rgb_composer(vec![&c1, &c2, &c3, &c4]).is_err());
    }

    #[test]
    fn combine_is_a_clamped_weighted_sum() {
        // Equal weights of 0.5 on 1 and 2 give 1.5, which must round up (truncation gives 1).
        assert_eq!(combine([1, 2, 0], 0.5, 0.5, 0.0), 2);
        assert_eq!(combine([1, 1, 0], 0.5, 0.5, 0.0), 1);
        // A lone weight dims, it is not renormalised away.
        assert_eq!(combine([1000, 0, 0], 0.5, 0.0, 0.0), 500);
        assert_eq!(combine([0, 0, 1000], 0.0, 0.0, 0.25), 250);
        // Weights past 1.0 clip at white instead of wrapping.
        assert_eq!(combine([MAX, MAX, 0], 1.0, 1.0, 0.0), MAX);
        assert_eq!(combine([40000, 40000, 40000], 1.0, 1.0, 1.0), MAX);
        // No weight, no signal.
        assert_eq!(combine([5, 5, 5], 0.0, 0.0, 0.0), 0);
    }

    #[test]
    fn new_channel_input_accepts_only_greyscale() {
        let [r, _, _] = real_planes();
        let ch = new_channel_input(r).unwrap();
        assert_eq!((ch.red, ch.green, ch.blue, ch.lum), (0.0, 0.0, 0.0, 0.0));

        assert!(new_channel_input(load_fits(&test_data("rgb.fits")).unwrap()).is_err());
        assert!(new_channel_input(load_fits(&test_data("cfa_orion.fits")).unwrap()).is_err());
    }

    // ---- remaining compose() paths ----

    /// A tiny non-square image, so a transposed row/column offset would show.
    fn small(template: &Image, pixels: PixelBuffer) -> Image {
        Image {
            width: 3,
            height: 2,
            ..mono_like(template, pixels)
        }
    }

    #[test]
    fn compose_absolute_weight_dims_a_single_input() {
        let [t, _, _] = real_planes();
        let n = t.width * t.height;
        let (c1, c2, c3) = (
            input(mono_like(&t, PixelBuffer::U16(vec![40000; n])), 0.5, 0.0, 0.0, 0.0),
            input(mono_like(&t, PixelBuffer::U16(vec![40000; n])), 0.0, 1.0, 0.0, 0.0),
            input(mono_like(&t, PixelBuffer::U16(vec![40000; n])), 0.0, 0.0, 2.0, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3]);
        for px in u16s(&out).chunks_exact(3) {
            assert_eq!(px, [20000, 40000, MAX]); // halved, untouched, clipped
        }
    }

    #[test]
    fn compose_non_square_image_keeps_pixel_positions() {
        let [t, _, _] = real_planes();
        let (c1, c2, c3) = (
            input(small(&t, PixelBuffer::U16(vec![1, 2, 3, 4, 5, 6])), 1.0, 0.0, 0.0, 0.0),
            input(small(&t, PixelBuffer::U16(vec![10, 20, 30, 40, 50, 60])), 0.0, 1.0, 0.0, 0.0),
            input(small(&t, PixelBuffer::U16(vec![100, 200, 300, 400, 500, 600])), 0.0, 0.0, 1.0, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3]);
        assert_eq!((out.width, out.height), (3, 2));
        assert_eq!(
            u16s(&out),
            [1, 10, 100, 2, 20, 200, 3, 30, 300, 4, 40, 400, 5, 50, 500, 6, 60, 600]
        );
    }

    #[test]
    fn compose_accepts_float_colour_inputs() {
        let [t, _, _] = real_planes();
        let n = t.width * t.height;
        // Normalised floats 0.0 / 1.0 become 0 / 65535.
        let f = |v: f32| mono_like(&t, PixelBuffer::F32(vec![v; n]));
        let (c1, c2, c3) = (
            input(f(1.0), 1.0, 0.0, 0.0, 0.0),
            input(f(0.0), 0.0, 1.0, 0.0, 0.0),
            input(f(1.0), 0.0, 0.0, 1.0, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3]);
        for px in u16s(&out).chunks_exact(3) {
            assert_eq!(px, [MAX, 0, MAX]);
        }
    }

    #[test]
    fn compose_accepts_a_16_bit_luminance_image() {
        let [t, _, _] = real_planes();
        let n = t.width * t.height;
        let grey = |v: u16| mono_like(&t, PixelBuffer::U16(vec![v; n]));
        // Black colour data + luminance at full weight: lightness is replaced entirely, and with
        // no chroma the result is the grey whose L* equals the luminance level.
        let (c1, c2, c3, l) = (
            input(grey(0), 1.0, 0.0, 0.0, 0.0),
            input(grey(0), 0.0, 1.0, 0.0, 0.0),
            input(grey(0), 0.0, 0.0, 1.0, 0.0),
            input(grey(MAX), 0.0, 0.0, 0.0, 1.0),
        );
        let out = compose(&[&c1, &c2, &c3, &l]);
        for px in u16s(&out).chunks_exact(3) {
            assert_eq!(px, [MAX, MAX, MAX]);
        }
    }

    #[test]
    fn compose_luminance_follows_pixel_positions() {
        let [t, _, _] = real_planes();
        let grey = |v: Vec<u16>| small(&t, PixelBuffer::U16(v));
        let (c1, c2, c3, l) = (
            input(grey(vec![0; 6]), 1.0, 0.0, 0.0, 0.0),
            input(grey(vec![0; 6]), 0.0, 1.0, 0.0, 0.0),
            input(grey(vec![0; 6]), 0.0, 0.0, 1.0, 0.0),
            input(grey(vec![0, MAX, 0, MAX, 0, MAX]), 0.0, 0.0, 0.0, 1.0),
        );
        let out = compose(&[&c1, &c2, &c3, &l]);
        let want: Vec<u16> = [0, MAX, 0, MAX, 0, MAX].iter().flat_map(|&v| [v; 3]).collect();
        assert_eq!(u16s(&out), want.as_slice());
    }

    #[test]
    fn compose_missing_exposure_counts_as_zero() {
        let bare = || Image::new(ImageType::Grayscale, fitskit::Header::new(), 1, 1, PixelBuffer::U16(vec![0]));
        let mut r = bare();
        r.header.set(EXPTIME, HeaderValue::Float(5.0), None);
        let (c1, c2, c3) = (
            input(r, 1.0, 0.0, 0.0, 0.0),
            input(bare(), 0.0, 1.0, 0.0, 0.0),
            input(bare(), 0.0, 0.0, 1.0, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3]);
        assert_eq!(out.header.get_float(EXPTIME), Some(5.0));
    }

    #[test]
    fn compose_includes_luminance_exposure_in_the_total() {
        let [mut r, mut g, mut b] = real_planes();
        let mut l = mono_like(&r, PixelBuffer::U16(vec![0; r.width * r.height]));
        for (img, t) in [(&mut r, 1.0), (&mut g, 2.0), (&mut b, 3.0), (&mut l, 4.0)] {
            img.header.set(EXPTIME, HeaderValue::Float(t), None);
        }
        let (c1, c2, c3, c4) = (
            input(r, 1.0, 0.0, 0.0, 0.0),
            input(g, 0.0, 1.0, 0.0, 0.0),
            input(b, 0.0, 0.0, 1.0, 0.0),
            input(l, 0.0, 0.0, 0.0, 0.0),
        );
        let out = compose(&[&c1, &c2, &c3, &c4]);
        assert_eq!(out.header.get_float(EXPTIME), Some(10.0));
    }
}
