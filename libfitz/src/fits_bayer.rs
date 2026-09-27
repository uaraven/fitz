use crate::xtrans::{self, XTransPattern};
use bayer::CFA;

pub(crate) enum BayerHeader {
    /// What a `BAYERPAT` header value resolved to: a standard 2x2 pattern, or an
    /// X-Trans 6x6 one: the 36-character pattern string GGRGGBGGBGGRBRGRBGGGBGGRGGRGGBRBGBRG
    Standard(CFA),
    XTrans(XTransPattern),
}

/// Parse a `BAYERPAT` value as either convention; length alone disambiguates
/// them (4 chars for a standard pattern name, 36 for an X-Trans tile).
pub(crate) fn parse_bayer_header(s: &str) -> Option<BayerHeader> {
    let s = s.trim();
    match s.len() {
        4 => parse_cfa(s).map(BayerHeader::Standard),
        36 => xtrans::parse_pattern(s).map(BayerHeader::XTrans),
        _ => None,
    }
}

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
        CFA::BGGR => "BGGR",
        CFA::GRBG => "GRBG",
    }
}
