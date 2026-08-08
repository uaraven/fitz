//! Copy FITS header keywords from a source image onto a target image, filling
//! in only the keywords the target doesn't already carry.

use crate::data::{Image, ImageType};
use crate::keywords::{CFA_KEYWORDS, copy_missing_metadata};

impl Image {
    /// Merge `other`'s image's header onto this, filling in only what this
    /// image doesn't already carry.
    ///
    /// A `BAYERPAT` (and related CFA keywords) from `source` is skipped when
    /// this images is not a CFA pattern.
    ///
    pub fn merge_headers_from(&mut self, other: &Image) {
        let extra_drop: &[&str] = if matches!(self.image_type, ImageType::CFA(_)) {
            &[]
        } else {
            CFA_KEYWORDS
        };
        copy_missing_metadata(&mut self.header, &other.header, extra_drop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::PixelBuffer;
    use bayer::CFA;
    use fitskit::{Header, HeaderValue, Keyword};

    fn image(image_type: ImageType) -> Image {
        Image::new(image_type, Header::new(), 1, 1, PixelBuffer::U16(vec![0]))
    }

    #[test]
    fn fills_in_missing_keyword() {
        let mut dest = image(ImageType::Grayscale);
        let mut src = image(ImageType::Grayscale);
        src.header.push(Keyword::with_value(
            "OBJECT",
            HeaderValue::String("M42".into()),
            None,
        ));

        dest.merge_headers_from(&src);

        assert_eq!(
            dest.header.get_string("OBJECT"),
            Some("M42"),
            "keyword absent from dest should be copied from src"
        );
    }

    #[test]
    fn does_not_overwrite_existing_keyword() {
        let mut dest = image(ImageType::Grayscale);
        dest.header.push(Keyword::with_value(
            "OBJECT",
            HeaderValue::String("Dest".into()),
            None,
        ));
        let mut src = image(ImageType::Grayscale);
        src.header.push(Keyword::with_value(
            "OBJECT",
            HeaderValue::String("Src".into()),
            None,
        ));

        dest.merge_headers_from(&src);

        assert_eq!(
            dest.header.get_string("OBJECT"),
            Some("Dest"),
            "a keyword dest already carries must not be overwritten"
        );
    }

    #[test]
    fn drops_cfa_keywords_when_dest_is_not_cfa() {
        let mut dest = image(ImageType::RGB);
        let mut src = image(ImageType::CFA(CFA::RGGB));
        src.header.push(Keyword::with_value(
            "BAYERPAT",
            HeaderValue::String("RGGB".into()),
            None,
        ));

        dest.merge_headers_from(&src);

        assert!(
            dest.header.find("BAYERPAT").is_none(),
            "BAYERPAT must not be copied onto a non-CFA image"
        );
    }

    #[test]
    fn keeps_cfa_keywords_when_dest_is_cfa() {
        let mut dest = image(ImageType::CFA(CFA::RGGB));
        let mut src = image(ImageType::CFA(CFA::RGGB));
        src.header.push(Keyword::with_value(
            "BAYERPAT",
            HeaderValue::String("RGGB".into()),
            None,
        ));

        dest.merge_headers_from(&src);

        assert_eq!(
            dest.header.get_string("BAYERPAT"),
            Some("RGGB"),
            "BAYERPAT should be copied when dest is itself a CFA image"
        );
    }

    #[test]
    fn skips_structural_keywords_from_src() {
        let mut dest = image(ImageType::Grayscale);
        let mut src = image(ImageType::Grayscale);
        src.header.push(Keyword::with_value(
            "BITPIX",
            HeaderValue::Integer(16),
            None,
        ));

        dest.merge_headers_from(&src);

        assert!(
            dest.header.find("BITPIX").is_none(),
            "structural keywords like BITPIX must never be copied"
        );
    }

    #[test]
    fn copies_commentary_keywords() {
        let mut dest = image(ImageType::Grayscale);
        let mut src = image(ImageType::Grayscale);
        src.header.push(Keyword::commentary("HISTORY", "processed"));

        dest.merge_headers_from(&src);

        let history = dest
            .header
            .find("HISTORY")
            .expect("HISTORY should be copied");
        assert_eq!(history.comment.as_deref(), Some("processed"));
    }
}
