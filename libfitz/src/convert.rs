

const U16_MAX_F32: f32 = u16::MAX as f32;

/// Normalize `[0, 1]` float sample to 0..65535
pub(crate) fn float_to_u16(v: f32) -> usize {
    (v * U16_MAX_F32).clamp(0.0, U16_MAX_F32) as usize
}

pub(crate) fn u16_to_float(v: u16) -> f32 {
    (v as f32) / U16_MAX_F32
}
