use fitskit::Header;

/// FITS header keywords used across the debayer/split commands.
pub const BAYERPAT: &str = "BAYERPAT";
pub const BSCALE: &str = "BSCALE";
pub const BZERO: &str = "BZERO";


/// Copy metadata keywords from `src` onto `dest`, skipping structural/reserved
/// keywords (see [`crate::fits_image::is_reserved_keyword`]) plus any names in `extra_drop`. `dest`
/// is expected to already carry the mandatory keywords that
/// `FitsFile::with_primary_image` generated (and any BSCALE/BZERO the writer
/// set); survivors are appended after them, preserving FITS keyword ordering.
/// Commentary cards (COMMENT/HISTORY, whose value is `None`) are copied verbatim
/// via `push` rather than `set`.
pub fn copy_metadata(dest: &mut Header, src: &Header, extra_drop: &[&str]) {
    for kw in &src.keywords {
        if is_droppable(&kw.name, extra_drop) {
            continue;
        }
        dest.push(kw.clone());
    }
}


/// True if a keyword must not be carried onto an output header: either a
/// structural/reserved keyword (see [`crate::fits_image::is_reserved_keyword`]) or one the caller
/// explicitly named in `extra_drop`. Shared by [`copy_metadata`] and
/// [`copy_missing_metadata`] so both apply the same drop rule.
fn is_droppable(name: &str, extra_drop: &[&str]) -> bool {
    is_reserved_keyword(name) || extra_drop.iter().any(|d| d.eq_ignore_ascii_case(name))
}


/// True if `name` is `prefix` followed by at least one ASCII digit and nothing
/// else (e.g. `is_indexed("NAXIS3", "NAXIS")` is true, but `"NAXIS"` and
/// `"NAXISA"` are false).
fn is_indexed(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}


/// True if `name` (uppercase, as fitskit stores keyword names) is a structural,
/// data-encoding, table, or tile-compression keyword that must not be copied
/// from a source header onto a freshly built output header: fitskit regenerates
/// the mandatory keywords for the new geometry, each writer sets its own
/// BSCALE/BZERO, and the table/`Z*` keywords only describe a compressed
/// container, not the image.
fn is_reserved_keyword(name: &str) -> bool {
    const EXACT: &[&str] = &[
        // Mandatory / structural.
        "SIMPLE", "BITPIX", "NAXIS", "EXTEND", "XTENSION", "PCOUNT", "GCOUNT",
        // Output encoding — owned by the writer.
        "BSCALE", "BZERO", // Data-dependent values tied to the old BITPIX / pixels.
        "BLANK", "DATAMIN", "DATAMAX", "CHECKSUM", "DATASUM",
        // BINTABLE structure (the compressed-image container).
        "TFIELDS", "THEAP", "EXTNAME", // Tile-compression scalar keywords.
        "ZIMAGE", "ZCMPTYPE", "ZBITPIX", "ZNAXIS", "ZQUANTIZ", "ZDITHER0", "ZBLANK", "ZMASKCMP",
        "ZSIMPLE", "ZEXTEND", "ZTENSION", "ZPCOUNT", "ZGCOUNT", "ZHECKSUM", "ZDATASUM",
        // Never copied as standalone cards.
        "END", "CONTINUE",
    ];
    if EXACT.contains(&name) {
        return true;
    }

    // Indexed families: <prefix> followed by one or more digits.
    const INDEXED: &[&str] = &[
        "NAXIS", "TFORM", "TTYPE", "TUNIT", "TSCAL", "TZERO", "TNULL", "TDIM", "TDISP", "ZNAXIS",
        "ZTILE", "ZNAME", "ZVAL",
    ];
    INDEXED.iter().any(|p| is_indexed(name, p))
}


/// Copy every non-structural keyword from `src` onto `dest` that `dest`
/// doesn't already carry a card for, returning the number of cards copied.
/// Used by the `copy-header` command to fill in only what a target file is
/// missing, rather than overwriting what it already has. `extra_drop` names
/// additional keywords to skip, on top of the reserved ones (see
/// [`copy_metadata`]) — `copy_header_file` uses this to keep a stale
/// `BAYERPAT` from a mosaic source landing on an already-debayered RGB target.
///
/// Structural/reserved keywords (see [`crate::fits_image::is_reserved_keyword`]) — `dest`'s own
/// resolution (`NAXIS*`), bit depth (`BITPIX`), pixel scaling
/// (`BSCALE`/`BZERO`), and similar data-layout keywords — are never copied,
/// since they describe `dest`'s own pixel data, not `src`'s. Commentary cards
/// (`COMMENT`/`HISTORY`, whose value is `None`) are always appended: multiple
/// independent annotations are normal, so they aren't deduplicated by name
/// like a regular keyword would be.
pub fn copy_missing_metadata(dest: &mut Header, src: &Header, extra_drop: &[&str]) -> usize {
    let mut copied = 0;
    for kw in &src.keywords {
        if is_droppable(&kw.name, extra_drop) {
            continue;
        }
        if kw.value.is_some() && dest.find(&kw.name).is_some() {
            continue;
        }
        dest.push(kw.clone());
        copied += 1;
    }
    copied
}


#[cfg(test)]
mod test {
    use crate::keywords::{is_indexed, is_reserved_keyword};

    #[test]
    fn is_indexed_matches_prefix_plus_digits_only() {
        assert!(is_indexed("NAXIS3", "NAXIS"));
        assert!(is_indexed("TFORM12", "TFORM"));
        assert!(is_indexed("ZNAXIS2", "ZNAXIS"));
        // bare prefix, non-digit suffix, and unrelated names do not match
        assert!(!is_indexed("NAXIS", "NAXIS"));
        assert!(!is_indexed("TFORMAT", "TFORM"));
        assert!(!is_indexed("OBJECT", "NAXIS"));
    }

    #[test]
    fn is_reserved_keyword_covers_structural_table_and_compression() {
        for kw in [
            "SIMPLE", "BITPIX", "NAXIS", "NAXIS1", "NAXIS3", "BSCALE", "BZERO", "BLANK", "DATAMIN",
            "CHECKSUM", "TFIELDS", "TFORM1", "TTYPE3", "EXTNAME", "ZIMAGE", "ZNAXIS", "ZNAXIS2",
            "ZTILE1", "ZVAL1", "END",
        ] {
            assert!(is_reserved_keyword(kw), "{kw} should be reserved");
        }
        for kw in [
            "OBJECT", "DATE-OBS", "CRVAL1", "BAYERPAT", "GAIN", "COMMENT", "HISTORY",
        ] {
            assert!(!is_reserved_keyword(kw), "{kw} should not be reserved");
        }
    }
}