use bayer::CFA;

pub(crate) fn parse_cfa(s: &str) -> Option<CFA> {
    match s.trim().to_ascii_uppercase().as_str() {
        "RGGB" => Some(CFA::RGGB),
        "GBRG" => Some(CFA::GBRG),
        "BGGR" => Some(CFA::BGGR),
        "GRBG" => Some(CFA::GRBG),
        _ => None,
    }
}

pub(crate) fn cfa_str(cfa: CFA) -> &'static str {
    match cfa {
        CFA::RGGB => "RGGB",
        CFA::GBRG => "GBRG",
        CFA::BGGR => "BGRG",
        CFA::GRBG => "GRBG",
    }
}
