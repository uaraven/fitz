//! The file list's frame-type icons and Selection ▸ by-type. Which type a frame
//! is — its `IMAGETYP` header, else its folders, else assumed a light — is
//! `libfitz::session`'s rule ([`resolve_frame_type`]); this module only maps
//! it onto the list rows: the icon's [`FrameKind`], its tooltip, the per-kind
//! counts the Selection menu shows, and checking the rows of one kind.

use std::path::Path;

use libfitz::session::{FrameType, FrameTypeSource, folder_frame_type, resolve_frame_type};
use slint::{Model, VecModel};

use crate::{FileRow, FrameCounts, FrameKind};

/// The list's icon kind for a `libfitz` frame type.
pub(crate) fn kind_of(frame_type: FrameType) -> FrameKind {
    match frame_type {
        FrameType::Light => FrameKind::Light,
        FrameType::Dark => FrameKind::Dark,
        FrameType::Bias => FrameKind::Bias,
        FrameType::Flat => FrameKind::Flat,
        FrameType::DarkFlat => FrameKind::DarkFlat,
    }
}

/// The icon's tooltip: the type's name — `"Dark frame"`, `"Dark flat frame"`.
pub(crate) fn kind_tip(frame_type: FrameType) -> String {
    let noun = frame_type.noun();
    format!("{}{} frame", noun[..1].to_ascii_uppercase(), &noun[1..])
}

/// Set `row`'s kind, tooltip and header flag from what its header says
/// (`header`, `None` when it names no type or has not been read yet) and the
/// folders above `path` up to `root`. Returns whether anything changed, so the
/// caller rewrites only rows that did.
pub(crate) fn classify_row(
    row: &mut FileRow,
    header: Option<FrameType>,
    path: &Path,
    root: &Path,
) -> bool {
    let (frame_type, source) = resolve_frame_type(header, folder_frame_type(path, root));
    let kind = kind_of(frame_type);
    let tip = kind_tip(frame_type);
    let from_header = source == FrameTypeSource::Header;
    if row.kind == kind && row.kind_from_header == from_header && row.kind_tip == tip {
        return false;
    }
    row.kind = kind;
    row.kind_from_header = from_header;
    row.kind_tip = tip.into();
    true
}

/// How many rows of each kind `model` holds.
pub(crate) fn count_kinds(model: &VecModel<FileRow>) -> FrameCounts {
    let mut counts = FrameCounts::default();
    for row in model.iter() {
        let slot = match row.kind {
            FrameKind::Light => &mut counts.light,
            FrameKind::Dark => &mut counts.dark,
            FrameKind::Bias => &mut counts.bias,
            FrameKind::Flat => &mut counts.flat,
            FrameKind::DarkFlat => &mut counts.dark_flat,
        };
        *slot += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_data;
    use crate::controller::tests::row;
    use crate::files::relative_names;
    use libfitz::files::common_dir;
    use libfitz::loader::load_header_from_file;
    use std::path::PathBuf;

    #[test]
    fn tooltips_name_the_type() {
        let tips: Vec<_> = FrameType::ALL.into_iter().map(kind_tip).collect();
        assert_eq!(
            tips,
            [
                "Light frame",
                "Dark frame",
                "Bias frame",
                "Flat frame",
                "Dark flat frame"
            ]
        );
    }

    #[test]
    fn every_frame_type_has_its_own_kind() {
        let kinds: Vec<_> = FrameType::ALL.into_iter().map(kind_of).collect();
        assert_eq!(
            kinds,
            [
                FrameKind::Light,
                FrameKind::Dark,
                FrameKind::Bias,
                FrameKind::Flat,
                FrameKind::DarkFlat
            ]
        );
    }

    #[test]
    fn a_real_headers_imagetyp_wins_over_its_folder() {
        // uncompressed.fit says `IMAGETYP= 'LIGHT'`; filed under Darks it is
        // still a light, and the row remembers the header decided it.
        let tmp = tempfile::tempdir().unwrap();
        let darks = tmp.path().join("Darks");
        std::fs::create_dir_all(&darks).unwrap();
        let path = darks.join("a.fit");
        std::fs::copy(test_data("uncompressed.fit"), &path).unwrap();
        let header = FrameType::from_header(&load_header_from_file(&path).unwrap());

        let mut r = row("a.fit");
        assert!(classify_row(&mut r, header, &path, tmp.path()));
        assert_eq!(r.kind, FrameKind::Light);
        assert!(r.kind_from_header);
        assert_eq!(r.kind_tip, "Light frame");
        // Same inputs again: nothing to rewrite.
        assert!(!classify_row(&mut r, header, &path, tmp.path()));
    }

    #[test]
    fn without_a_header_type_the_folder_decides_up_to_the_root() {
        let paths = [
            PathBuf::from("/d/night1/Darks/a.fit"),
            PathBuf::from("/d/night1/Flats/b.fit"),
            PathBuf::from("/d/night1/c.fit"),
        ];
        let root = common_dir(&paths);
        let mut r = row("a.fit");
        classify_row(&mut r, None, &paths[0], &root);
        assert_eq!((r.kind, r.kind_from_header), (FrameKind::Dark, false));
        assert_eq!(r.kind_tip, "Dark frame");
        classify_row(&mut r, None, &paths[2], &root);
        assert_eq!((r.kind, r.kind_from_header), (FrameKind::Light, false));
        assert_eq!(r.kind_tip, "Light frame");

        // With only the dark in the set, its own folder is the root, and the
        // root's own name still counts.
        let alone = common_dir(&paths[..1]);
        assert_eq!(relative_names(&paths[..1]), ["a.fit"]);
        classify_row(&mut r, None, &paths[0], &alone);
        assert_eq!(r.kind, FrameKind::Dark);
    }

    #[test]
    fn counts_tally_each_kind() {
        let kinds = [
            FrameKind::Light,
            FrameKind::Light,
            FrameKind::Dark,
            FrameKind::DarkFlat,
        ];
        let model = VecModel::from(
            kinds
                .iter()
                .map(|&kind| FileRow { kind, ..row("x") })
                .collect::<Vec<_>>(),
        );
        let counts = count_kinds(&model);
        assert_eq!(
            (
                counts.light,
                counts.dark,
                counts.bias,
                counts.flat,
                counts.dark_flat
            ),
            (2, 1, 0, 0, 1)
        );
    }
}
