use crate::convert::float_to_u16;
use crate::data::{Image, ImageType, PixelBuffer};
use rayon::prelude::*;

const BIN_COUNT: usize = 65536;
const HISTOGRAM_BIN_COUNT: usize = 256;

const RGB_CHANNEL_COUNT: usize = 3;

pub struct Stats {
    /// mean value of pixels
    pub mean: f32,
    /// median value of pixels (approximate)
    pub median: f32,
    /// standard deviation
    pub sigma: f32,
    /// average deviation
    pub avg_dev: f32,
    /// median average deviation
    pub mad: f32,
    /// min value of pixels
    pub min: u16,
    /// max value of pixels
    pub max: u16,
    /// number of pixels with min value
    pub min_count: u64,
    /// number of pixels with max value
    pub max_count: u64,
    /// mode - the most common pixel value
    pub mode: u16,
    /// number of pixels with zero value (crunched shadows)
    pub zero_count: u32,
    /// number of pixels at full scale for `estimated_bit_depth` (blown highlights)
    pub saturated_count: u32,
    /// sample bit depth (8, 10, 12, 14 or 16), guessed from the max pixel value
    pub estimated_bit_depth: usize,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            mean: 0.0,
            median: 0.0,
            sigma: 0.0,
            avg_dev: 0.0,
            mad: 0.0,
            min: 0,
            max: 0,
            min_count: 0,
            max_count: 0,
            mode: 0,
            zero_count: 0,
            saturated_count: 0,
            estimated_bit_depth: 0,
        }
    }
}

pub struct ImageStats {
    pub channels: Vec<Stats>,
    pub histogram: [u64; HISTOGRAM_BIN_COUNT],
}

impl Default for ImageStats {
    fn default() -> Self {
        Self {
            channels: vec![Stats::default()],
            histogram: [0u64; HISTOGRAM_BIN_COUNT],
        }
    }
}

impl Image {
    pub fn stats(&self) -> ImageStats {
        match self.image_type {
            ImageType::CFA(_) | ImageType::Grayscale => single_channel_stats(&self.pixels),
            ImageType::RGB => self.multi_channel_stats(),
        }
    }

    fn multi_channel_stats(&self) -> ImageStats {
        let counts = match &self.pixels {
            PixelBuffer::U16(px) => interleaved_counts(px, |&v| v as usize),
            PixelBuffer::F32(px) => interleaved_counts(px, |&v| float_to_u16(v)),
        };
        let stats = counts.chunks_exact(BIN_COUNT).map(process).collect();
        ImageStats {
            channels: stats,
            histogram: histogram_from_rgb(&self.pixels),
        }
    }
}

/// Width, in raw 16-bit sample values, of one [`HISTOGRAM_BIN_COUNT`] bucket —
/// the same bucketing `histogram_from_pixel_values` applies to a per-value
/// count table, applied here directly to each pixel's luma.
const HISTOGRAM_BUCKET: usize = BIN_COUNT / HISTOGRAM_BIN_COUNT;

fn histogram_from_rgb(pix: &PixelBuffer) -> [u64; HISTOGRAM_BIN_COUNT] {
    let accumulator = |mut a: Vec<u64>, b: Vec<u64>| {
        for (slot, c) in a.iter_mut().zip(b) {
            *slot += c;
        }
        a
    };
    let result = match pix {
        PixelBuffer::U16(pixels) => pixels
            .par_chunks(RGB_CHANNEL_COUNT)
            .fold(
                || vec![0u64; HISTOGRAM_BIN_COUNT],
                |mut histogram, px| {
                    let luma = (px[0] as f32 * 0.2126
                        + px[1] as f32 * 0.7152
                        + px[2] as f32 * 0.0722) as u16 as usize;
                    histogram[luma / HISTOGRAM_BUCKET] += 1;
                    histogram
                },
            )
            .reduce(|| vec![0u64; HISTOGRAM_BIN_COUNT], accumulator),
        PixelBuffer::F32(pixels) => pixels
            .par_chunks(RGB_CHANNEL_COUNT)
            .fold(
                || vec![0u64; HISTOGRAM_BIN_COUNT],
                |mut histogram, px| {
                    let luma = float_to_u16(px[0] * 0.2126 + px[1] * 0.7152 + px[2] * 0.0722);
                    histogram[luma / HISTOGRAM_BUCKET] += 1;
                    histogram
                },
            )
            .reduce(|| vec![0u64; HISTOGRAM_BIN_COUNT], accumulator),
    };
    result.try_into().unwrap()
}

pub(crate) fn single_channel_stats(pixels: &PixelBuffer) -> ImageStats {
    let counts = single_channel_px_counts(pixels);
    let stats = process(&counts);
    ImageStats {
        channels: vec![stats],
        histogram: histogram_from_pixel_values(&counts),
    }
}

/// Calculates the numbers of the 16-bit mono pixels in image
/// Returns 65536-value vector which is essentially a histogram of the image
fn single_channel_px_counts(pixels: &PixelBuffer) -> Vec<u32> {
    let counts = match pixels {
        PixelBuffer::U16(px) => count_into_bins(px, |&v| v as usize),
        PixelBuffer::F32(px) => count_into_bins(px, |&v| float_to_u16(v)),
    };
    counts
}

/// Chunk length for the counting passes below: one chunk per thread, but never
/// too short
fn count_chunk(len: usize) -> usize {
    len.div_ceil(rayon::current_num_threads())
        .max(4 * BIN_COUNT)
}

/// Count `values` into a `BIN_COUNT`-slot table, `bin` mapping a sample to its
/// slot. One parallel pass, no per-pixel allocation.
fn count_into_bins<T: Sync>(values: &[T], bin: impl Fn(&T) -> usize + Sync + Send) -> Vec<u32> {
    values
        .par_chunks(count_chunk(values.len()))
        .fold(
            || vec![0u32; BIN_COUNT],
            |mut acc, chunk| {
                for v in chunk {
                    acc[bin(v)] += 1;
                }
                acc
            },
        )
        .reduce(|| vec![0u32; BIN_COUNT], add_counts)
}

/// Count three interleaved channels in a single pass, into one table per
/// channel laid end to end (channel `c` occupies `c * BIN_COUNT ..`).
fn interleaved_counts<T: Sync>(samples: &[T], bin: impl Fn(&T) -> usize + Sync + Send) -> Vec<u32> {
    let chunk = count_chunk(samples.len() / 3) * 3;
    samples
        .par_chunks(chunk)
        .fold(
            || vec![0u32; 3 * BIN_COUNT],
            |mut acc, chunk| {
                for px in chunk.chunks_exact(3) {
                    acc[bin(&px[0])] += 1;
                    acc[BIN_COUNT + bin(&px[1])] += 1;
                    acc[2 * BIN_COUNT + bin(&px[2])] += 1;
                }
                acc
            },
        )
        .reduce(|| vec![0u32; 3 * BIN_COUNT], add_counts)
}

/// Sum two equal-length count tables element-wise, reusing `a`'s allocation.
fn add_counts(mut a: Vec<u32>, b: Vec<u32>) -> Vec<u32> {
    for (slot, c) in a.iter_mut().zip(b) {
        *slot += c;
    }
    a
}

fn process(collector: &[u32]) -> Stats {
    let min = collector.iter().position(|&x| x != 0).unwrap_or(0) as u16;
    let max = collector
        .iter()
        .rposition(|&x| x != 0)
        .unwrap_or(BIN_COUNT - 1) as u16;
    let estimated_bit_depth = estimate_bit_depth(max);

    let (total_count, mean, avg_dev, sigma, mode) = mean_and_sigma(collector);
    let median = median_from_counts(collector, total_count);
    let mad = mad_from_counts(collector, total_count, median);
    let min_count = collector[min as usize] as u64;
    let max_count = collector[max as usize] as u64;
    Stats {
        mean,
        median,
        sigma,
        avg_dev,
        mad,
        min,
        max,
        min_count,
        max_count,
        mode,
        zero_count: collector[0],
        estimated_bit_depth,
        saturated_count: collector[full_scale(estimated_bit_depth) as usize],
    }
}

/// Sample count, mean, and population standard deviation, exact from the
/// per-value counts (`counts[v]` = number of pixels with value `v`).
/// returns:
///  - total number of elements (pixels)
///  - mean
///  - avg deviation
///  - std deviation
///  - mode
fn mean_and_sigma(counts: &[u32]) -> (u64, f32, f32, f32, u16) {
    let mut largest_count = 0;
    let mut mode = 0;
    let mut n: u64 = 0;
    let mut sum: u128 = 0; // Σ v·counts[v]
    let mut sum_sq: u128 = 0; // Σ v²·counts[v]
    for (value, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        if count > largest_count {
            largest_count = count;
            mode = value;
        }
        let (v, c) = (value as u128, count as u128);
        n += count as u64;
        sum += v * c;
        sum_sq += v * v * c;
    }
    if n == 0 {
        return (0, 0.0, 0.0, 0.0, 0);
    }
    let mean = sum as f32 / n as f32;
    let sigma = ((n as u128 * sum_sq - sum * sum) as f32).sqrt() / n as f32;

    // Average deviation Σ|v − mean| from the counts alone: values split into a
    // group below the mean and one above it, each contributing a signed sum that
    // needs only that group's count and value sum.
    let split = (mean.floor() as usize).min(counts.len() - 1);
    let (mut n_lo, mut s_lo) = (0u64, 0u64);
    for (value, &count) in counts[..=split].iter().enumerate() {
        let c = count as u64;
        n_lo += c;
        s_lo += value as u64 * c;
    }
    let s_hi = (sum - s_lo as u128) as f32;
    let sum_abs = (mean * n_lo as f32 - s_lo as f32) + (s_hi - mean * (n - n_lo) as f32);

    (n, mean, sum_abs / n as f32, sigma, mode as u16)
}

/// Sample bit depths a raw frame plausibly comes from, ascending.
const STANDARD_BIT_DEPTHS: [usize; 5] = [8, 10, 12, 14, 16];

/// Guess the sample bit depth from the brightest pixel: the narrowest standard
/// depth whose full scale still covers `max`.
///
/// Cameras store sub-16-bit samples unshifted in 16-bit words, so a 12-bit
/// sensor's blown highlights sit at 4095 and never reach 65535 — counting
/// saturation at the top of the u16 range would report none at all.
pub(crate) fn estimate_bit_depth(max: u16) -> usize {
    STANDARD_BIT_DEPTHS
        .into_iter()
        .find(|&bits| max <= full_scale(bits))
        .unwrap_or(16)
}

/// The largest value representable in `bits` bits.
pub(crate) fn full_scale(bits: usize) -> u16 {
    ((1u32 << bits) - 1) as u16
}

fn median_from_counts(counts: &[u32], n: u64) -> f32 {
    if n == 0 {
        return 0.0;
    }
    let lower_rank = (n - 1) / 2; // 0-indexed rank of the lower central sample
    let upper_rank = n / 2; // == lower_rank when n is odd
    let mut cumulative = 0u64;
    let mut lower = 0u64;
    for (value, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let before = cumulative;
        cumulative += count as u64;
        if before <= lower_rank && cumulative > lower_rank {
            lower = value as u64;
        }
        if cumulative > upper_rank {
            return (lower + value as u64) as f32 / 2.0;
        }
    }
    unreachable!("cumulative count reaches n")
}

fn mad_from_counts(counts: &[u32], n: u64, median: f32) -> f32 {
    // The walk below only terminates because `median` came from these very
    // counts, so expanding outwards from it is guaranteed to reach half the
    // samples before either end runs off the table.
    debug_assert!(
        median.floor() >= 0.0 && (median.floor() as usize) < counts.len(),
        "median {median} is outside the count table"
    );
    let (mut lo, mut hi) = (median.floor() as isize, median.floor() as isize + 1);
    let mut covered = 0u64;
    loop {
        let (dlo, dhi) = (median - lo as f32, hi as f32 - median);
        let take_lo = lo >= 0 && (dlo <= dhi || hi >= counts.len() as isize);
        let d = if take_lo {
            covered += counts[lo as usize] as u64;
            lo -= 1;
            dlo
        } else {
            covered += counts[hi as usize] as u64;
            hi += 1;
            dhi
        };
        if covered * 2 >= n {
            return d;
        }
    }
}

/// Reduces the raw per-value counts (one entry per possible u16 pixel value) to a 256-element
/// histogram
fn histogram_from_pixel_values(counts: &[u32]) -> [u64; HISTOGRAM_BIN_COUNT] {
    let bucket_size = counts.len() / HISTOGRAM_BIN_COUNT;

    let mut histogram = [0u64; HISTOGRAM_BIN_COUNT];
    for (bin, chunk) in histogram.iter_mut().zip(counts.chunks_exact(bucket_size)) {
        *bin = chunk.iter().map(|&c| c as u64).sum();
    }
    histogram
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::ImageType;
    use crate::fits_file::load_fits;
    use crate::test_support::test_data;
    use fitskit::Header;

    fn make_image(pixels: Vec<u16>) -> Image {
        let len = pixels.len();
        Image::new(
            ImageType::Grayscale,
            Header::new(),
            len,
            1,
            PixelBuffer::U16(pixels),
        )
    }

    fn make_rgb_image(interleaved: PixelBuffer, width: usize, height: usize) -> Image {
        Image::new(ImageType::RGB, Header::new(), width, height, interleaved)
    }

    /// The obvious serial value-count array, used to check the parallel
    /// counting kernels and to drive the statistics helpers directly from
    /// sample lists.
    fn naive_counts(pixels: &[u16]) -> Vec<u32> {
        let mut counts = vec![0u32; BIN_COUNT];
        for &p in pixels {
            counts[p as usize] += 1;
        }
        counts
    }

    /// The u16 view of an image's pixels, for tests that compare against a
    /// naive computation over the raw samples.
    fn u16_pixels(pixels: &PixelBuffer) -> Vec<u16> {
        match pixels {
            PixelBuffer::U16(px) => px.clone(),
            PixelBuffer::F32(px) => px.iter().map(|&v| float_to_u16(v) as u16).collect(),
        }
    }

    #[test]
    fn process_computes_mean_min_max_and_value_counts() {
        let img = make_image(vec![1, 2, 2, 3, 3, 3, 10, 10]);
        let stats = single_channel_stats(&img.pixels);
        let channel = &stats.channels[0];

        assert_eq!(channel.min, 1);
        assert_eq!(channel.max, 10);
        assert_eq!(channel.min_count, 1);
        assert_eq!(channel.max_count, 2);
        assert!((channel.mean - 4.25).abs() < 1e-5);

        // The two extreme bins are empty even though `max_count` is not: the
        // brightest value here is 10, nowhere near the top of the range.
        assert_eq!(channel.zero_count, 0);
        assert_eq!(channel.saturated_count, 0);

        // All 8 samples are < 256, so they land in the histogram's first bin.
        assert_eq!(stats.histogram[0], 8);
        assert_eq!(stats.histogram[1..].iter().sum::<u64>(), 0);
    }

    #[test]
    fn process_converts_f32_pixels_via_u16_quantization() {
        // pixels_u16 scales by 65535 and truncates, so 0.5 lands on 32767, not 32768.
        let img = Image::new(
            crate::data::ImageType::Grayscale,
            Header::new(),
            3,
            1,
            PixelBuffer::F32(vec![0.0, 0.5, 1.0]),
        );
        let stats = single_channel_stats(&img.pixels);
        let channel = &stats.channels[0];

        assert_eq!(channel.min, 0);
        assert_eq!(channel.max, 65535);
        assert_eq!(channel.min_count, 1);
        assert_eq!(channel.max_count, 1);
        let expected_mean = (0u32 + 32767 + 65535) as f32 / 3.0;
        assert!((channel.mean - expected_mean).abs() < 1e-3);

        // 0.0 lands in bin 0 and 1.0 clamps into the top bin.
        assert_eq!(channel.zero_count, 1);
        assert_eq!(channel.saturated_count, 1);

        // bucket = value / 256: 0 -> bin 0, 32767 -> bin 127, 65535 -> bin 255.
        assert_eq!(stats.histogram[0], 1);
        assert_eq!(stats.histogram[127], 1);
        assert_eq!(stats.histogram[255], 1);
        assert_eq!(stats.histogram.iter().sum::<u64>(), 3);
    }

    #[test]
    fn single_channel_stats_matches_naive_computation_on_real_data() {
        let img = load_fits(&test_data("uncompressed.fit")).unwrap();
        let pixels = u16_pixels(&img.pixels);

        let expected_min = *pixels.iter().min().unwrap();
        let expected_max = *pixels.iter().max().unwrap();
        let expected_min_count = pixels.iter().filter(|&&p| p == expected_min).count() as u64;
        let expected_max_count = pixels.iter().filter(|&&p| p == expected_max).count() as u64;
        let expected_zero = pixels.iter().filter(|&&p| p == 0).count() as u32;
        // This frame reaches 65532, so its full scale is the top of the u16
        // range — see `estimated_bit_depth` below.
        let expected_saturated = pixels.iter().filter(|&&p| p == u16::MAX).count() as u32;
        let expected_sum: u64 = pixels.iter().map(|&p| p as u64).sum();
        let expected_mean = expected_sum as f32 / pixels.len() as f32;

        let stats = single_channel_stats(&img.pixels);
        let channel = &stats.channels[0];

        assert_eq!(channel.min, expected_min);
        assert_eq!(channel.max, expected_max);
        assert_eq!(channel.min_count, expected_min_count);
        assert_eq!(channel.max_count, expected_max_count);
        assert_eq!(channel.zero_count, expected_zero);
        assert_eq!(channel.estimated_bit_depth, 16);
        assert_eq!(channel.saturated_count, expected_saturated);
        assert!((channel.mean - expected_mean).abs() < 1.0);

        assert_eq!(stats.histogram.iter().sum::<u64>(), pixels.len() as u64);
        assert_eq!(
            stats.histogram,
            histogram_from_pixel_values(&naive_counts(&pixels))
        );
    }

    /// The bundled frames are unclipped (`uncompressed.fit` spans 228..=65532),
    /// so their zero/saturated counts are both 0. Rescaling the real pixels onto
    /// the full u16 range reproduces the clipped-shadow / blown-highlight frame
    /// these two counts exist to report, keeping a real pixel distribution.
    #[test]
    fn zero_and_saturated_counts_on_a_real_frame_stretched_to_full_range() {
        let img = load_fits(&test_data("uncompressed.fit")).unwrap();
        let pixels = u16_pixels(&img.pixels);
        let lo = *pixels.iter().min().unwrap();
        let hi = *pixels.iter().max().unwrap();
        let span = (hi - lo) as u64;

        // lo maps exactly onto 0 and hi exactly onto 65535; nothing else does.
        let stretched: Vec<u16> = pixels
            .iter()
            .map(|&p| ((p - lo) as u64 * 65535 / span) as u16)
            .collect();
        let stats = single_channel_stats(&PixelBuffer::U16(stretched));
        let channel = &stats.channels[0];

        let expected_zero = pixels.iter().filter(|&&p| p == lo).count() as u32;
        let expected_saturated = pixels.iter().filter(|&&p| p == hi).count() as u32;
        assert!(expected_zero > 0 && expected_saturated > 0);

        assert_eq!(channel.zero_count, expected_zero);
        assert_eq!(channel.saturated_count, expected_saturated);
        // Now that both extremes are populated they agree with the range counts.
        assert_eq!(channel.zero_count as u64, channel.min_count);
        assert_eq!(channel.saturated_count as u64, channel.max_count);
    }

    #[test]
    fn mean_sigma_and_deviations_match_hand_computed_values() {
        // 1, 2, 2, 3, 10: mean = 3.6, median = 2.
        let counts = naive_counts(&[1, 2, 2, 3, 10]);
        let (n, mean, avg_dev, sigma, mode) = mean_and_sigma(&counts);

        assert_eq!(n, 5);
        assert!((mean - 3.6).abs() < 1e-4);
        assert!((sigma - 3.261_902).abs() < 1e-3);
        assert!((avg_dev - 2.56).abs() < 1e-4);
        assert_eq!(mode, 2);
        assert!((mad_from_counts(&counts, n, 2.0) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn deviations_are_zero_for_constant_data() {
        let counts = naive_counts(&[7, 7, 7, 7]);
        let (n, mean, avg_dev, sigma, mode) = mean_and_sigma(&counts);

        assert_eq!(mean, 7.0);
        assert_eq!(sigma, 0.0);
        assert_eq!(avg_dev, 0.0);
        assert_eq!(mode, 7);
        assert_eq!(mad_from_counts(&counts, n, 7.0), 0.0);
    }

    #[test]
    fn statistics_of_an_empty_frame_are_zero() {
        let counts = vec![0u32; BIN_COUNT];
        let stats = process(&counts);

        assert_eq!(mean_and_sigma(&counts), (0, 0.0, 0.0, 0.0, 0));
        assert_eq!(median_from_counts(&counts, 0), 0.0);
        assert_eq!(mad_from_counts(&counts, 0, 0.0), 0.0);
        // No populated value: min falls back to 0 and max to the last bin.
        assert_eq!((stats.min, stats.max), (0, 65535));
        assert_eq!((stats.min_count, stats.max_count), (0, 0));
        // …and that fallback must not be mistaken for saturation, even though
        // it makes the depth estimate read as a full 16 bits.
        assert_eq!(stats.estimated_bit_depth, 16);
        assert_eq!((stats.zero_count, stats.saturated_count), (0, 0));
    }

    #[test]
    fn zero_and_saturated_counts_track_the_extreme_bins() {
        let img = make_image(vec![0, 0, 0, 5, 40000, 65535, 65535]);
        let stats = single_channel_stats(&img.pixels);
        let channel = &stats.channels[0];

        assert_eq!(channel.zero_count, 3);
        assert_eq!(channel.saturated_count, 2);

        // With neither extreme present both counts are zero, even though the
        // frame still has a minimum and a maximum: these count the clipping
        // levels, not the observed range. 40000 needs all 16 bits, so full
        // scale is 65535 and no pixel sits there.
        let img = make_image(vec![100, 40000]);
        let stats = single_channel_stats(&img.pixels);
        let channel = &stats.channels[0];

        assert_eq!(channel.estimated_bit_depth, 16);
        assert_eq!((channel.min_count, channel.max_count), (1, 1));
        assert_eq!((channel.zero_count, channel.saturated_count), (0, 0));
    }

    #[test]
    fn bit_depth_is_the_narrowest_standard_depth_covering_the_maximum() {
        // Boundaries: full scale of a depth still fits it, one more does not.
        for (max, expected) in [
            (0u16, 8usize),
            (1, 8),
            (255, 8),
            (256, 10),
            (1023, 10),
            (1024, 12),
            (4095, 12),
            (4096, 14),
            (16383, 14),
            (16384, 16),
            (65534, 16),
            (65535, 16),
        ] {
            assert_eq!(estimate_bit_depth(max), expected, "max {max}");
            assert!(max <= full_scale(estimate_bit_depth(max)), "max {max}");
        }

        assert_eq!(full_scale(8), 255);
        assert_eq!(full_scale(12), 4095);
        assert_eq!(full_scale(16), 65535);
    }

    #[test]
    fn saturation_is_counted_at_the_estimated_depth_not_at_the_top_of_the_u16_range() {
        // A 12-bit sensor's blown highlights sit at 4095; counting only bin
        // 65535 (the old rule) would report none of them.
        let img = make_image(vec![0, 100, 4000, 4095, 4095, 4095]);
        let stats = single_channel_stats(&img.pixels);
        let channel = &stats.channels[0];

        assert_eq!(channel.estimated_bit_depth, 12);
        assert_eq!(channel.max, 4095);
        assert_eq!(channel.saturated_count, 3);
        // Everything at full scale is also the maximum, so the two agree.
        assert_eq!(channel.saturated_count as u64, channel.max_count);

        // One count below full scale is not saturation, even though it is
        // still the brightest pixel in the frame.
        let img = make_image(vec![0, 100, 4000, 4094]);
        let stats = single_channel_stats(&img.pixels);
        let channel = &stats.channels[0];

        assert_eq!(channel.estimated_bit_depth, 12);
        assert_eq!(channel.max_count, 1);
        assert_eq!(channel.saturated_count, 0);
    }

    #[test]
    fn saturation_level_follows_each_standard_depth() {
        // One frame per depth, three pixels pinned at that depth's full scale
        // plus a mid-range pixel that must never be counted.
        for bits in STANDARD_BIT_DEPTHS {
            let sat = full_scale(bits);
            let img = make_image(vec![sat / 2, sat, sat, sat]);
            let stats = single_channel_stats(&img.pixels);
            let channel = &stats.channels[0];

            assert_eq!(channel.estimated_bit_depth, bits, "depth {bits}");
            assert_eq!(channel.saturated_count, 3, "depth {bits}");
        }
    }

    #[test]
    fn each_channel_saturates_at_its_own_estimated_depth() {
        // R stays 8-bit and is clipped, G reaches 12-bit full scale, B is
        // 16-bit but unclipped — one estimate per channel, not per frame.
        let interleaved = vec![
            255u16, 4095, 40000, //
            255, 4095, 40000, //
            10, 2000, 50000,
        ];
        let stats = make_rgb_image(PixelBuffer::U16(interleaved), 3, 1).multi_channel_stats();
        let channels = &stats.channels;

        assert_eq!(
            [
                channels[0].estimated_bit_depth,
                channels[1].estimated_bit_depth,
                channels[2].estimated_bit_depth
            ],
            [8, 12, 16]
        );
        assert_eq!(
            [
                channels[0].saturated_count,
                channels[1].saturated_count,
                channels[2].saturated_count
            ],
            [2, 2, 0]
        );
    }

    /// `uncompressed.fit` spans 228..=65532, so at 16 bits nothing is clipped.
    /// Shifting it down to 12 bits reproduces a real 12-bit frame — same pixel
    /// distribution — whose brightest pixels now land exactly on full scale.
    #[test]
    fn saturated_count_on_a_real_frame_shifted_to_twelve_bits() {
        let img = load_fits(&test_data("uncompressed.fit")).unwrap();
        let pixels = u16_pixels(&img.pixels);
        assert_eq!(
            single_channel_stats(&img.pixels).channels[0].estimated_bit_depth,
            16
        );
        assert_eq!(
            single_channel_stats(&img.pixels).channels[0].saturated_count,
            0
        );

        let shifted: Vec<u16> = pixels.iter().map(|&p| p >> 4).collect();
        let expected_saturated = shifted.iter().filter(|&&p| p == 4095).count() as u32;
        let expected_zero = shifted.iter().filter(|&&p| p == 0).count() as u32;
        assert!(expected_saturated > 0, "the shifted frame must clip");

        let stats = single_channel_stats(&PixelBuffer::U16(shifted));
        let channel = &stats.channels[0];

        assert_eq!(channel.estimated_bit_depth, 12);
        assert_eq!(channel.max, 4095);
        assert_eq!(channel.saturated_count, expected_saturated);
        assert_eq!(channel.saturated_count as u64, channel.max_count);
        assert_eq!(channel.zero_count, expected_zero);
    }

    #[test]
    fn median_takes_the_central_sample_and_averages_the_two_central_ones() {
        let odd = naive_counts(&[1, 2, 2, 3, 10]);
        assert_eq!(median_from_counts(&odd, 5), 2.0);

        // Even sample count: the two central samples are 2 and 3.
        let even = naive_counts(&[1, 2, 3, 10]);
        assert_eq!(median_from_counts(&even, 4), 2.5);

        // A single populated value is its own median.
        let mut constant = vec![0u32; BIN_COUNT];
        constant[500] = 7;
        assert_eq!(median_from_counts(&constant, 7), 500.0);
    }

    #[test]
    fn mad_is_the_half_covering_distance_from_the_median() {
        // 10 x2, 20 x3, 30 x1: median 20, deviations 10,10,0,0,0,10 -> MAD 0.
        let mut counts = vec![0u32; BIN_COUNT];
        counts[10] = 2;
        counts[20] = 3;
        counts[30] = 1;
        assert_eq!(mad_from_counts(&counts, 6, 20.0), 0.0);

        // Drop the middle bin: 10 x2, 30 x1, median 10, half of 3 samples is
        // covered by the median bin alone.
        counts[20] = 0;
        assert_eq!(mad_from_counts(&counts, 3, 10.0), 0.0);

        // 10 x1, 30 x2, median 30: covering half of 3 needs the 10s as well.
        counts[10] = 1;
        counts[30] = 2;
        assert_eq!(mad_from_counts(&counts, 3, 30.0), 0.0);
    }

    #[test]
    fn statistics_match_a_naive_computation_on_real_data() {
        let img = load_fits(&test_data("uncompressed.fit")).unwrap();
        let pixels = u16_pixels(&img.pixels);
        let mut sorted = pixels.to_vec();
        sorted.sort_unstable();

        let stats = single_channel_stats(&img.pixels);
        let channel = &stats.channels[0];

        let expected_median = if sorted.len() % 2 == 1 {
            sorted[sorted.len() / 2] as f32
        } else {
            (sorted[sorted.len() / 2 - 1] as f32 + sorted[sorted.len() / 2] as f32) / 2.0
        };
        assert_eq!(channel.median, expected_median);

        let mean = sorted.iter().map(|&p| p as f64).sum::<f64>() / sorted.len() as f64;
        let variance = sorted
            .iter()
            .map(|&p| (p as f64 - mean).powi(2))
            .sum::<f64>()
            / sorted.len() as f64;
        assert!((channel.sigma as f64 - variance.sqrt()).abs() < 1.0);

        let expected_avg_dev =
            sorted.iter().map(|&p| (p as f64 - mean).abs()).sum::<f64>() / sorted.len() as f64;
        assert!((channel.avg_dev as f64 - expected_avg_dev).abs() < 1.0);

        // MAD: the smallest distance from the median covering half the samples.
        let mut deviations: Vec<f32> = sorted
            .iter()
            .map(|&p| (p as f32 - expected_median).abs())
            .collect();
        deviations.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!((channel.mad - deviations[(deviations.len() - 1) / 2]).abs() <= 1.0);
    }

    #[test]
    fn histogram_from_pixel_values_sums_each_256_wide_bucket() {
        let mut counts = vec![0u32; BIN_COUNT];
        counts[0] = 3;
        counts[1] = 1;
        counts[255] = 2; // last value of bucket 0
        counts[256] = 5; // first value of bucket 1
        counts[65535] = 7; // last value of the last bucket

        let histogram = histogram_from_pixel_values(&counts);

        assert_eq!(histogram.len(), HISTOGRAM_BIN_COUNT);
        assert_eq!(histogram[0], 6);
        assert_eq!(histogram[1], 5);
        assert_eq!(histogram[HISTOGRAM_BIN_COUNT - 1], 7);
        assert_eq!(histogram.iter().sum::<u64>(), 18);
    }

    #[test]
    fn histogram_from_pixel_values_matches_naive_bucket_sum_on_real_data() {
        let img = load_fits(&test_data("uncompressed.fit")).unwrap();
        let pixels = u16_pixels(&img.pixels);

        let counts = naive_counts(&pixels);
        // The parallel chunked counter must agree with the serial one.
        assert_eq!(count_into_bins(&pixels, |&v| v as usize), counts);

        let bucket_size = BIN_COUNT / HISTOGRAM_BIN_COUNT;
        let expected: Vec<u64> = counts
            .chunks(bucket_size)
            .map(|chunk| chunk.iter().map(|&c| c as u64).sum())
            .collect();

        let histogram = histogram_from_pixel_values(&counts);

        assert_eq!(histogram.to_vec(), expected);
        assert_eq!(histogram.iter().sum::<u64>(), pixels.len() as u64);
    }

    /// The chunked counter splits the frame across threads, so its result must
    /// not depend on where the chunk boundaries fall.
    #[test]
    fn interleaved_counts_match_a_naive_per_channel_count_on_real_data() {
        let loaded = load_fits(&test_data("cfa_orion.fits")).unwrap();
        let rgb = loaded.debayer().unwrap().unwrap();
        let PixelBuffer::U16(interleaved) = &rgb.pixels else {
            panic!("expected u16 pixels from debayer");
        };

        let counts = interleaved_counts(interleaved, |&v| v as usize);

        for (channel, table) in counts.chunks_exact(BIN_COUNT).enumerate() {
            let plane: Vec<u16> = interleaved
                .iter()
                .skip(channel)
                .step_by(3)
                .copied()
                .collect();
            assert_eq!(table, naive_counts(&plane), "channel {channel}");
        }
    }

    /// Counting f32 samples directly must land them in exactly the bins the
    /// quantized u16 copy would have — that equivalence is the whole reason the
    /// float path no longer materializes one.
    #[test]
    fn counting_floats_directly_matches_counting_the_quantized_u16_copy() {
        let img = load_fits(&test_data("uncompressed.fit")).unwrap();
        let PixelBuffer::U16(px) = &img.pixels else {
            panic!("expected u16 pixels");
        };
        // Round-trip the real frame into normalized floats, the form an I32/I64
        // or float FITS source would have loaded as.
        let floats: Vec<f32> = px.iter().map(|&v| v as f32 / 65535.0).collect();

        let from_floats = count_into_bins(&floats, |&v| float_to_u16(v));
        let quantized: Vec<u16> = floats.iter().map(|&v| float_to_u16(v) as u16).collect();

        assert_eq!(from_floats, naive_counts(&quantized));
        assert_eq!(
            from_floats.iter().map(|&c| c as u64).sum::<u64>(),
            px.len() as u64
        );
    }

    /// The naive per-pixel luma computation [`histogram_from_rgb`] performs in
    /// one parallel pass over interleaved R,G,B triplets, used to check its
    /// result independently of that pass's chunking.
    fn naive_rgb_luma_histogram(interleaved: &[u16]) -> [u64; HISTOGRAM_BIN_COUNT] {
        let mut histogram = [0u64; HISTOGRAM_BIN_COUNT];
        for px in interleaved.chunks_exact(RGB_CHANNEL_COUNT) {
            let luma = (px[0] as f32 * 0.2126 + px[1] as f32 * 0.7152 + px[2] as f32 * 0.0722)
                as u16 as usize;
            histogram[luma / HISTOGRAM_BUCKET] += 1;
        }
        histogram
    }

    #[test]
    fn multi_channel_stats_splits_interleaved_u16_rgb_into_per_channel_stats() {
        // 3 pixels, interleaved as R,G,B triplets.
        let interleaved = vec![10, 100, 1000, 20, 200, 2000, 30, 300, 3000];
        let expected_histogram = naive_rgb_luma_histogram(&interleaved);
        let img = make_rgb_image(PixelBuffer::U16(interleaved), 3, 1);

        let stats = img.multi_channel_stats();
        assert_eq!(stats.channels.len(), 3);

        assert_eq!(stats.channels[0].min, 10);
        assert_eq!(stats.channels[0].max, 30);
        assert!((stats.channels[0].mean - 20.0).abs() < 1e-5);

        assert_eq!(stats.channels[1].min, 100);
        assert_eq!(stats.channels[1].max, 300);
        assert!((stats.channels[1].mean - 200.0).abs() < 1e-5);

        assert_eq!(stats.channels[2].min, 1000);
        assert_eq!(stats.channels[2].max, 3000);
        assert!((stats.channels[2].mean - 2000.0).abs() < 1e-5);

        // One whole-image luminosity histogram, not one per channel: each of
        // the 3 pixels contributes a single Rec. 709 luma sample rather than
        // 3 separate per-plane samples.
        assert_eq!(stats.histogram, expected_histogram);
        assert_eq!(stats.histogram.iter().sum::<u64>(), 3);
    }

    #[test]
    fn multi_channel_stats_scales_interleaved_f32_rgb_samples_to_u16() {
        // f32 samples in 0..=1 are scaled to the 0..=65535 range before stats
        // are computed, same conversion as the mono `pixels_u16` path.
        let interleaved = vec![0.0, 0.5, 1.0, 1.0, 0.5, 0.0];
        let img = make_rgb_image(PixelBuffer::F32(interleaved), 2, 1);

        let stats = img.multi_channel_stats();
        assert_eq!(stats.channels.len(), 3);

        assert_eq!(stats.channels[0].min, 0);
        assert_eq!(stats.channels[0].max, 65535);
        assert_eq!(stats.channels[1].min, 32767);
        assert_eq!(stats.channels[1].max, 32767);
        assert_eq!(stats.channels[2].min, 0);
        assert_eq!(stats.channels[2].max, 65535);

        // Each channel's extreme bins are read from its own count table: the
        // outer channels hold one 0.0 and one 1.0 each, the middle one neither.
        assert_eq!(
            (
                stats.channels[0].zero_count,
                stats.channels[0].saturated_count
            ),
            (1, 1)
        );
        assert_eq!(
            (
                stats.channels[1].zero_count,
                stats.channels[1].saturated_count
            ),
            (0, 0)
        );
        assert_eq!(
            (
                stats.channels[2].zero_count,
                stats.channels[2].saturated_count
            ),
            (1, 1)
        );

        // 2 pixels, so 2 luma samples in the whole-image histogram.
        assert_eq!(stats.histogram.iter().sum::<u64>(), 2);
    }

    #[test]
    fn multi_channel_stats_matches_naive_per_channel_computation_on_real_debayered_data() {
        let loaded = load_fits(&test_data("cfa_orion.fits")).unwrap();
        let rgb = loaded.debayer().unwrap().unwrap();
        let PixelBuffer::U16(interleaved) = &rgb.pixels else {
            panic!("expected u16 pixels from debayer");
        };

        let stats = rgb.multi_channel_stats();
        assert_eq!(stats.channels.len(), 3);

        for (channel, stat) in stats.channels.iter().enumerate() {
            let plane: Vec<u16> = interleaved
                .iter()
                .skip(channel)
                .step_by(3)
                .copied()
                .collect();

            let expected_min = *plane.iter().min().unwrap();
            let expected_max = *plane.iter().max().unwrap();
            let expected_zero = plane.iter().filter(|&&p| p == 0).count() as u32;
            // The naive counterpart of the production rule, spelled out so the
            // expectation does not lean on the helpers under test: the
            // narrowest standard depth the plane's maximum fits into, and the
            // pixels sitting on that depth's full scale.
            let expected_depth = [8u32, 10, 12, 14, 16]
                .into_iter()
                .find(|&bits| (expected_max as u32) < (1u32 << bits))
                .unwrap();
            let expected_full_scale = ((1u32 << expected_depth) - 1) as u16;
            let expected_saturated =
                plane.iter().filter(|&&p| p == expected_full_scale).count() as u32;
            let expected_sum: u64 = plane.iter().map(|&p| p as u64).sum();
            let expected_mean = expected_sum as f32 / plane.len() as f32;

            assert_eq!(stat.min, expected_min);
            assert_eq!(stat.max, expected_max);
            assert_eq!(stat.zero_count, expected_zero, "channel {channel}");
            assert_eq!(
                stat.estimated_bit_depth, expected_depth as usize,
                "channel {channel}"
            );
            assert_eq!(
                stat.saturated_count, expected_saturated,
                "channel {channel}"
            );
            assert!((stat.mean - expected_mean).abs() < 1.0);
        }

        // One whole-image luminosity histogram, one luma sample per pixel
        // (not per channel), matching the naive per-pixel computation.
        let pixel_count = interleaved.len() / RGB_CHANNEL_COUNT;
        assert_eq!(stats.histogram.iter().sum::<u64>(), pixel_count as u64);
        assert_eq!(stats.histogram, naive_rgb_luma_histogram(interleaved));
    }
}
