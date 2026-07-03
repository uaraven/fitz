//! Encode an RGB image into the [kitty graphics
//! protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) so a capable
//! terminal can display it inline at full pixel fidelity.
//!
//! This module is pure: it produces the escape-sequence string and does no
//! terminal I/O or capability detection (that lives in [`crate::terminal`]).

use std::fmt::Write;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Maximum base64 payload bytes per APC frame, per the protocol's chunking rule.
const CHUNK: usize = 4096;

/// Build the kitty-graphics escape sequence that transmits and displays an
/// interleaved 8-bit RGB image of `width` x `height` pixels (`f=24`).
///
/// The base64 payload is split into <=4096-byte chunks across one or more APC
/// frames: the first frame carries the format/size keys, every frame but the
/// last sets `m=1`, and the final frame sets `m=0`.
pub fn encode_image(rgb8: &[u8], width: usize, height: usize) -> String {
    let payload = STANDARD.encode(rgb8);
    let bytes = payload.as_bytes();
    // Number of frames to emit: one per chunk, but at least one even for an
    // empty image so the escape sequence is still well-formed.
    let frames = bytes.len().div_ceil(CHUNK).max(1);

    let mut out = String::with_capacity(payload.len() + frames * 24);
    let mut chunks = bytes.chunks(CHUNK);
    for i in 0..frames {
        // Empty payload yields no chunk for the single frame we still emit.
        let chunk = chunks.next().unwrap_or(b"");
        out.push_str("\x1b_G");
        if i == 0 {
            // First frame carries the display action, pixel format and size.
            let _ = write!(out, "a=T,f=24,s={width},v={height},");
        }
        // Every frame but the last sets m=1 to signal a continuation.
        let _ = write!(out, "m={};", (i < frames - 1) as u8);
        // base64 output is ASCII, so the chunk is always valid UTF-8.
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push_str("\x1b\\");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fits_image::rgb16_to_rgb8;
    use crate::preview::scale_rgb_to_fit;
    use crate::stretch::load_and_stretch;
    use crate::test_support::test_data;

    fn frames(seq: &str) -> Vec<&str> {
        // Split on the APC terminator, dropping the empty tail after the last one.
        seq.split("\x1b\\").filter(|s| !s.is_empty()).collect()
    }

    #[test]
    fn single_chunk_image_is_one_frame_with_m0() {
        // 1x1 pixel -> 3 bytes -> 4 base64 chars, well under one chunk.
        let seq = encode_image(&[10, 20, 30], 1, 1);
        let fr = frames(&seq);
        assert_eq!(fr.len(), 1);
        assert!(fr[0].starts_with("\x1b_Ga=T,f=24,s=1,v=1,m=0;"));
        let payload = fr[0].rsplit(';').next().unwrap();
        assert_eq!(STANDARD.decode(payload).unwrap(), vec![10, 20, 30]);
    }

    #[test]
    fn large_image_is_chunked_with_continuation_flags() {
        // Enough pixels that the base64 payload spans several 4096-byte chunks.
        let rgb8 = vec![123u8; CHUNK * 3 + 99];
        let seq = encode_image(&rgb8, 64, 53);
        let fr = frames(&seq);
        assert!(fr.len() >= 2, "expected multiple frames, got {}", fr.len());

        // First frame: format keys + continuation flag; size matches dimensions.
        assert!(fr[0].starts_with("\x1b_Ga=T,f=24,s=64,v=53,m=1;"));
        // Middle frames carry only m=1 (no format keys); last carries m=0.
        for f in &fr[1..fr.len() - 1] {
            assert!(f.starts_with("\x1b_Gm=1;"));
        }
        assert!(fr.last().unwrap().starts_with("\x1b_Gm=0;"));

        // Every frame's payload stays within the chunk limit, and concatenated
        // they decode back to the original bytes.
        let mut joined = String::new();
        for f in &fr {
            let payload = f.rsplit(';').next().unwrap();
            assert!(payload.len() <= CHUNK);
            joined.push_str(payload);
        }
        assert_eq!(STANDARD.decode(joined).unwrap(), rgb8);
    }

    #[test]
    fn encodes_real_image_and_round_trips() {
        // Full pipeline on a bundled frame: load -> stretch -> scale -> encode,
        // then decode the payload back and confirm it equals the 8-bit RGB.
        let input = test_data("uncompressed.fit");
        let (w, h, stretched, _) = load_and_stretch(
            &input,
            None,
            false,
            false,
            crate::stretch::DEFAULT_BRIGHTNESS,
            false,
        )
        .unwrap();
        let (pw, ph, preview) = scale_rgb_to_fit(&stretched, w, h, 120, 120);

        let rgb8 = rgb16_to_rgb8(&preview);
        let seq = encode_image(&rgb8, pw, ph);
        let joined: String = frames(&seq)
            .iter()
            .map(|f| f.rsplit(';').next().unwrap())
            .collect();
        assert_eq!(STANDARD.decode(joined).unwrap(), rgb8);
        assert!(seq.contains(&format!("s={pw},v={ph}")));
    }
}
