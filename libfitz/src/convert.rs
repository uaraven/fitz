/// Normalize `[0, 1]` float sample to 0..65535
pub(crate) fn float_to_u16(v: f32) -> usize {
    (v * 65535.0).clamp(0.0, 65535.0) as usize
}

pub(crate) fn u16_to_float(v: u16) -> f32 {
    (v as f32) / 65535.0
}
