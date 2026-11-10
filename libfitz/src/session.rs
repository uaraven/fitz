//! Grouping a tree of astro images into observing sessions, frame types and
//! filters, with frame counts and integration times at every level.
//!
//! Every image under a root folder is scanned — FITS and camera RAW alike,
//! whose EXIF is translated to FITS-style keywords by
//! [`crate::loader::load_header_from_file`]. For each image:
//!
//! - the **frame type** comes from the folder names: the nearest enclosing
//!   folder (up to and including the scan root) whose name is a known type
//!   (`lights`, `darks`, `biases`, `flats`, … in any case) decides it, and an
//!   image with no such folder is a [`FrameType::Light`];
//! - the **filter** comes from the `FILTER` header (`None` when absent or
//!   blank — always the case for RAW);
//! - the **session** is the observation *night* of its timestamp (`DATE-LOC`,
//!   else `DATE-OBS`), cut at local noon so a night that crosses midnight
//!   stays in one session. Frames without a readable timestamp share one
//!   "unknown date" session.
//!
//! The result is a [`Summary`] → [`Session`] → [`TypeGroup`] → [`FilterGroup`]
//! → [`Frame`] tree. Every group above a frame records its frame count and
//! total exposure ("integration") time. A [`Session`] and the [`Summary`]
//! report integration for light frames only, the figure that matters for an
//! astrophoto; their frame counts cover every type.

use crate::files::find_images;
use crate::fits_file::parse_date_obs;
use crate::keywords::{DATE_LOC, DATE_OBS, EXPTIME, FILTER};
use crate::loader::load_header_from_file;
use crate::non_blank;
use anyhow::{Result, bail};
use fitskit::Header;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SECONDS_PER_DAY: f64 = 86_400.0;
/// Local noon: nights are cut here, so 22:00 and 02:00 the next morning are the
/// same night, while 13:00 starts a new one.
const NIGHT_CUTOFF: f64 = SECONDS_PER_DAY / 2.0;

/// Extensions the open-file dialogs accept as RAW but a session scan skips: a
/// `.tif` in an imaging tree is a processed export or a stacker's output, not
/// a camera frame, and LibRaw rejects it ("FileUnsupported"), which would only
/// add noise to the summary's error list.
const SKIPPED_EXTENSIONS: &[&str] = &["tif"];

/// What an image is for. Ordered as they are listed in a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FrameType {
    /// Science exposures of the target.
    Light,
    /// Same exposure as the lights, shutter closed: thermal signal.
    Dark,
    /// Shortest possible exposure, shutter closed: readout signal.
    Bias,
    /// Evenly lit field, for vignetting and dust correction.
    Flat,
    /// Shutter closed at the flats' exposure time, to calibrate the flats.
    DarkFlat,
}

impl FrameType {
    /// The type a folder name stands for, ignoring case and surrounding
    /// whitespace (`Lights`, `LIGHT`, `biases`, `dark_flats`, …); `None` for
    /// any other name.
    pub fn from_folder_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "light" | "lights" => Some(Self::Light),
            "dark" | "darks" => Some(Self::Dark),
            "bias" | "biases" => Some(Self::Bias),
            "flat" | "flats" => Some(Self::Flat),
            "darkflat" | "darkflats" | "dark_flat" | "dark_flats" => Some(Self::DarkFlat),
            _ => None,
        }
    }

    /// Singular noun a frame of this type is counted by (`"dark"` in "12 dark
    /// frames", `"dark flat"`).
    pub fn noun(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Bias => "bias",
            Self::Flat => "flat",
            Self::DarkFlat => "dark flat",
        }
    }

    /// Plural display label (`"Lights"`, `"Dark flats"`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Light => "Lights",
            Self::Dark => "Darks",
            Self::Bias => "Biases",
            Self::Flat => "Flats",
            Self::DarkFlat => "Dark flats",
        }
    }
}

/// One image: where it is, how long it exposed for, and when it was taken.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The image file.
    pub path: PathBuf,
    /// `EXPTIME` in seconds; 0 when the header has none.
    pub exposure: f64,
    /// Observation time as a Unix-epoch timestamp (`DATE-LOC`, else
    /// `DATE-OBS`, read as written with no timezone shift); `None` when
    /// neither parses.
    pub observed: Option<f64>,
}

/// The frames of one type taken through one filter.
#[derive(Debug, Clone)]
pub struct FilterGroup {
    /// The trimmed `FILTER` value; `None` for frames without one.
    pub filter: Option<String>,
    /// Ordered by observation time, then path.
    pub frames: Vec<Frame>,
    /// Number of frames.
    pub count: usize,
    /// Total exposure, seconds.
    pub integration: f64,
}

/// Shown in place of a filter name for frames whose header has none (always the
/// case for camera RAW).
const NO_FILTER: &str = "(no filter)";

impl FilterGroup {
    /// The filter's name, or `"(no filter)"` for frames without one.
    pub fn label(&self) -> &str {
        self.filter.as_deref().unwrap_or(NO_FILTER)
    }

    fn new(filter: Option<String>, mut frames: Vec<Frame>) -> Self {
        frames.sort_by(|a, b| {
            cmp_option_last(a.observed, b.observed, f64::total_cmp)
                .then_with(|| a.path.cmp(&b.path))
        });
        Self {
            filter,
            count: frames.len(),
            integration: frames.iter().map(|f| f.exposure).sum(),
            frames,
        }
    }
}

/// All the frames of one [`FrameType`], split by filter.
#[derive(Debug, Clone)]
pub struct TypeGroup {
    /// The frame type every frame in the group has.
    pub frame_type: FrameType,
    /// In [`filter_rank`] order: L, R, G, B, Ha, SII, OIII, any other filter
    /// alphabetically (ignoring case), and frames without a filter last.
    pub filters: Vec<FilterGroup>,
    /// Number of frames over all `filters`.
    pub count: usize,
    /// Total exposure, seconds: the sum over `filters`.
    pub integration: f64,
}

impl TypeGroup {
    fn new(frame_type: FrameType, filters: Vec<FilterGroup>) -> Self {
        Self {
            frame_type,
            count: filters.iter().map(|g| g.count).sum(),
            integration: filters.iter().map(|g| g.integration).sum(),
            filters,
        }
    }

    /// Every frame of this type, filter by filter.
    pub fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.filters.iter().flat_map(|g| g.frames.iter())
    }
}

/// Everything captured during one observing night.
#[derive(Debug, Clone)]
pub struct Session {
    /// The night's starting day as days since the Unix epoch (see
    /// [`night_of`]); `None` for the frames without a readable timestamp.
    pub night: Option<i64>,
    /// In [`FrameType`] order; only the types that have frames.
    pub types: Vec<TypeGroup>,
    /// Frames of every type.
    pub count: usize,
    /// Total exposure of the light frames only, seconds.
    pub integration: f64,
    /// Earliest observation time of any frame in the session (Unix epoch);
    /// `None` when no frame has a readable timestamp.
    pub start: Option<f64>,
    /// Latest observation time of any frame in the session; `None` like `start`.
    pub end: Option<f64>,
}

impl Session {
    fn new(night: Option<i64>, types: Vec<TypeGroup>) -> Self {
        let observed = types
            .iter()
            .flat_map(|t| t.frames())
            .filter_map(|f| f.observed);
        let (start, end) =
            observed.fold((None, None), |(lo, hi): (Option<f64>, Option<f64>), t| {
                (
                    Some(lo.map_or(t, |lo| lo.min(t))),
                    Some(hi.map_or(t, |hi| hi.max(t))),
                )
            });
        Self {
            night,
            count: types.iter().map(|t| t.count).sum(),
            integration: types
                .iter()
                .find(|t| t.frame_type == FrameType::Light)
                .map_or(0.0, |t| t.integration),
            start,
            end,
            types,
        }
    }

    /// The night's starting date (`"2026-06-22"`), or `"unknown date"`.
    pub fn label(&self) -> String {
        match self.night {
            Some(day) => {
                let (y, m, d) = civil_from_days(day);
                format!("{y:04}-{m:02}-{d:02}")
            }
            None => "unknown date".to_string(),
        }
    }

    /// The group of `frame_type`, if the session has any such frames.
    pub fn type_group(&self, frame_type: FrameType) -> Option<&TypeGroup> {
        self.types.iter().find(|t| t.frame_type == frame_type)
    }

    /// Every frame in the session, type by type.
    pub fn frames(&self) -> impl Iterator<Item = &Frame> {
        self.types.iter().flat_map(|t| t.frames())
    }
}

/// The result of [`summarize_dir`].
#[derive(Debug, Clone)]
pub struct Summary {
    /// The scanned folder.
    pub root: PathBuf,
    /// Chronological, the unknown-date session last.
    pub sessions: Vec<Session>,
    /// Frames of every type in every session.
    pub count: usize,
    /// Total exposure of the light frames only, seconds.
    pub integration: f64,
    /// Images that could not be read, with the reason. They are left out of
    /// `sessions`; one bad file never aborts the scan.
    pub errors: Vec<(PathBuf, String)>,
}

impl Summary {
    fn new(root: PathBuf, sessions: Vec<Session>, errors: Vec<(PathBuf, String)>) -> Self {
        Self {
            root,
            count: sessions.iter().map(|s| s.count).sum(),
            integration: sessions.iter().map(|s| s.integration).sum(),
            sessions,
            errors,
        }
    }

    /// Number of light frames over all sessions.
    pub fn light_count(&self) -> usize {
        self.sessions
            .iter()
            .filter_map(|s| s.type_group(FrameType::Light))
            .map(|t| t.count)
            .sum()
    }
}

/// An image found by [`scan_images`] with the type its folders give it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedImage {
    /// The image file.
    pub path: PathBuf,
    /// The type its enclosing folders give it (see [`frame_type_for`]).
    pub frame_type: FrameType,
}

/// The frame type for the image at `path` under `root`: the nearest folder
/// above the image, up to and including `root` itself, whose name is a known
/// type. [`FrameType::Light`] when none is.
pub fn frame_type_for(path: &Path, root: &Path) -> FrameType {
    frame_type_in(path, root, folder_type(root))
}

/// [`frame_type_for`] with the root folder's own type already resolved, so a
/// scan resolves it once rather than per image.
fn frame_type_in(path: &Path, root: &Path, root_type: Option<FrameType>) -> FrameType {
    for dir in path.ancestors().skip(1) {
        if dir == root {
            return root_type.unwrap_or(FrameType::Light);
        }
        if let Some(frame_type) = folder_type(dir) {
            return frame_type;
        }
    }
    FrameType::Light
}

/// The type a folder's own name stands for. A root such as `.` has no name of
/// its own, so it is resolved through its canonical path.
fn folder_type(dir: &Path) -> Option<FrameType> {
    let name = match dir.file_name() {
        Some(name) => name.to_owned(),
        None => dir.canonicalize().ok()?.file_name()?.to_owned(),
    };
    FrameType::from_folder_name(name.to_str()?)
}

/// Every FITS and RAW image anywhere under `root`, in path order, each with its
/// folder-derived frame type. No file is opened. Folders that cannot be read
/// are skipped; symlinked folders are not followed (so a link cannot loop).
pub fn scan_images(root: &Path) -> Vec<ScannedImage> {
    let root_type = folder_type(root);
    find_images(root, SKIPPED_EXTENSIONS)
        .into_iter()
        .map(|path| ScannedImage {
            frame_type: frame_type_in(&path, root, root_type),
            path,
        })
        .collect()
}

/// Scan every image under `root` (see the module docs) and group it into
/// sessions, frame types and filters. Headers are read in parallel and never
/// decode pixels. Fails only if `root` is not a directory.
pub fn summarize_dir(root: &Path) -> Result<Summary> {
    if !root.is_dir() {
        bail!("not a directory");
    }
    Ok(summarize_images(root.to_path_buf(), scan_images(root)))
}

/// Group an explicit list of image files — a working set rather than a folder
/// tree — the same way [`summarize_dir`] does. With no folder to scan, the
/// frame type comes from the folders above each file up to their deepest
/// common folder (which becomes [`Summary::root`] and whose own name counts,
/// as a scan root's does). Files that cannot be read land in
/// [`Summary::errors`]; an empty list gives an empty summary.
pub fn summarize_files(paths: &[PathBuf]) -> Summary {
    let root = common_dir(paths);
    let root_type = folder_type(&root);
    let images = paths
        .iter()
        .map(|path| ScannedImage {
            frame_type: frame_type_in(path, &root, root_type),
            path: path.clone(),
        })
        .collect();
    summarize_images(root, images)
}

/// The deepest folder containing every path (empty when they share none, or
/// when there are no paths).
fn common_dir(paths: &[PathBuf]) -> PathBuf {
    let mut dirs = paths.iter().map(|p| p.parent().unwrap_or(Path::new("")));
    let Some(first) = dirs.next() else {
        return PathBuf::new();
    };
    let mut common = first.to_path_buf();
    for dir in dirs {
        while !dir.starts_with(&common) {
            if !common.pop() {
                return PathBuf::new();
            }
        }
    }
    common
}

/// Read the headers of `images` (in parallel) and bucket them into a
/// [`Summary`] rooted at `root`.
fn summarize_images(root: PathBuf, images: Vec<ScannedImage>) -> Summary {
    let read: Vec<(ScannedImage, Result<FrameInfo>)> = images
        .into_par_iter()
        .map(|image| {
            let info = read_frame_info(&image.path);
            (image, info)
        })
        .collect();

    // night → type → filter → frames. A leading `bool` on a night key sorts the
    // unknown date last; the filter key is its `filter_rank`, then its name
    // ignoring case (the name itself keeps differently-cased spellings apart).
    type Frames = BTreeMap<(usize, Option<String>, Option<String>), Vec<Frame>>;
    let mut nights: BTreeMap<(bool, Option<i64>), BTreeMap<FrameType, Frames>> = BTreeMap::new();
    let mut errors = Vec::new();
    for (image, info) in read {
        match info {
            Ok(info) => {
                let night = info.frame.observed.map(night_of);
                nights
                    .entry((night.is_none(), night))
                    .or_default()
                    .entry(image.frame_type)
                    .or_default()
                    .entry((
                        filter_rank(info.filter.as_deref()),
                        info.filter.as_deref().map(str::to_lowercase),
                        info.filter,
                    ))
                    .or_default()
                    .push(info.frame);
            }
            Err(e) => errors.push((image.path, format!("{e:#}"))),
        }
    }

    let sessions = nights
        .into_iter()
        .map(|((_, night), types)| {
            let types = types
                .into_iter()
                .map(|(frame_type, filters)| {
                    let filters = filters
                        .into_iter()
                        .map(|((_, _, filter), frames)| FilterGroup::new(filter, frames))
                        .collect();
                    TypeGroup::new(frame_type, filters)
                })
                .collect();
            Session::new(night, types)
        })
        .collect();

    Summary::new(root, sessions, errors)
}

/// The filters listed ahead of all others, in this order (compared ignoring
/// case): luminance, the RGB trio, then the narrowband Ha, SII, OIII.
const FILTER_ORDER: [&str; 7] = ["L", "R", "G", "B", "Ha", "SII", "OIII"];

/// Where a filter sorts among the filter groups of a type: its position in
/// [`FILTER_ORDER`], then every other named filter (which the caller orders
/// alphabetically, ignoring case), then frames without a filter.
fn filter_rank(filter: Option<&str>) -> usize {
    match filter {
        Some(name) => FILTER_ORDER
            .iter()
            .position(|known| known.eq_ignore_ascii_case(name))
            .unwrap_or(FILTER_ORDER.len()),
        None => FILTER_ORDER.len() + 1,
    }
}

/// What the headers of one image contribute to a summary.
struct FrameInfo {
    frame: Frame,
    filter: Option<String>,
}

fn read_frame_info(path: &Path) -> Result<FrameInfo> {
    let header = load_header_from_file(path)?;
    Ok(FrameInfo {
        frame: Frame {
            path: path.to_path_buf(),
            exposure: header
                .get_float(EXPTIME)
                .filter(|e| e.is_finite() && *e > 0.0)
                .unwrap_or(0.0),
            observed: observed_time(&header),
        },
        filter: header
            .get_string(FILTER)
            .and_then(non_blank)
            .map(str::to_owned),
    })
}

/// `DATE-LOC` if it parses, otherwise `DATE-OBS`.
fn observed_time(header: &Header) -> Option<f64> {
    [DATE_LOC, DATE_OBS].into_iter().find_map(|key| {
        header
            .get_string(key)
            .and_then(non_blank)
            .and_then(parse_date_obs)
    })
}

/// The observation night a timestamp belongs to: the day (since the Unix
/// epoch) of the most recent local noon at or before it. 22:00 on the 22nd and
/// 02:00 on the 23rd are both night 22; 13:00 on the 23rd is night 23.
pub fn night_of(observed: f64) -> i64 {
    ((observed - NIGHT_CUTOFF) / SECONDS_PER_DAY).floor() as i64
}

/// Compare two options with `None` after every `Some`.
fn cmp_option_last<T: Copy>(
    a: Option<T>,
    b: Option<T>,
    cmp: impl Fn(&T, &T) -> Ordering,
) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => cmp(&a, &b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// A duration as `"5h 12m 30s"` (minutes and seconds zero-padded, hours not
/// capped), rounded to the nearest second.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!(
        "{}h {:02}m {:02}s",
        total / 3600,
        total % 3600 / 60,
        total % 60
    )
}

/// A Unix-epoch timestamp as `"2026-06-22 21:30"` (to the minute, no timezone:
/// header times are read as written).
pub fn format_timestamp(epoch: f64) -> String {
    let minutes = (epoch / 60.0).floor() as i64;
    let (y, m, d) = civil_from_days(minutes.div_euclid(24 * 60));
    let of_day = minutes.rem_euclid(24 * 60);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", of_day / 60, of_day % 60)
}

/// The civil date for a count of days since the Unix epoch — the inverse of
/// `fits_file::days_from_civil` (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // Mar=0 .. Feb=11
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_data, write_frame_fits};
    use std::fs;
    use tempfile::TempDir;

    /// `root/<rel>` as a tiny FITS frame; creates the folders on the way.
    fn frame(
        root: &Path,
        rel: &str,
        filter: Option<&str>,
        exptime: f64,
        date_loc: Option<&str>,
        date_obs: Option<&str>,
    ) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_frame_fits(&path, filter, exptime, date_loc, date_obs);
        path
    }

    fn names(summary: &Summary) -> Vec<String> {
        summary.sessions.iter().map(Session::label).collect()
    }

    #[test]
    fn folder_names_map_to_frame_types_ignoring_case() {
        for name in ["light", "Lights", "LIGHTS", "LiGhTs", " lights "] {
            assert_eq!(
                FrameType::from_folder_name(name),
                Some(FrameType::Light),
                "{name}"
            );
        }
        assert_eq!(FrameType::from_folder_name("Dark"), Some(FrameType::Dark));
        assert_eq!(FrameType::from_folder_name("DARKS"), Some(FrameType::Dark));
        assert_eq!(FrameType::from_folder_name("bias"), Some(FrameType::Bias));
        assert_eq!(FrameType::from_folder_name("Biases"), Some(FrameType::Bias));
        assert_eq!(FrameType::from_folder_name("flat"), Some(FrameType::Flat));
        assert_eq!(FrameType::from_folder_name("FLATS"), Some(FrameType::Flat));
        assert_eq!(
            FrameType::from_folder_name("DarkFlats"),
            Some(FrameType::DarkFlat)
        );
        assert_eq!(
            FrameType::from_folder_name("dark_flat"),
            Some(FrameType::DarkFlat)
        );
        assert_eq!(FrameType::from_folder_name("M31"), None);
        assert_eq!(FrameType::from_folder_name(""), None);
    }

    #[test]
    fn frame_type_comes_from_the_nearest_known_ancestor() {
        let root = Path::new("/data/M31");
        let of = |p: &str| frame_type_for(Path::new(p), root);

        assert_eq!(of("/data/M31/darks/a.fit"), FrameType::Dark);
        // An unknown folder is walked past.
        assert_eq!(of("/data/M31/darks/batch1/a.fit"), FrameType::Dark);
        assert_eq!(of("/data/M31/Flats/Ha/night1/a.fit"), FrameType::Flat);
        // The nearest known folder wins over a farther one.
        assert_eq!(of("/data/M31/lights/darks/a.fit"), FrameType::Dark);
        assert_eq!(of("/data/M31/darks/lights/a.fit"), FrameType::Light);
        // No known folder at all: the default.
        assert_eq!(of("/data/M31/a.fit"), FrameType::Light);
        assert_eq!(of("/data/M31/night1/a.fit"), FrameType::Light);
    }

    #[test]
    fn the_scan_roots_own_name_counts_but_nothing_above_it() {
        let darks = Path::new("/data/darks");
        assert_eq!(
            frame_type_for(Path::new("/data/darks/a.fit"), darks),
            FrameType::Dark
        );
        assert_eq!(
            frame_type_for(Path::new("/data/darks/x/a.fit"), darks),
            FrameType::Dark
        );

        // `darks` is above the root, so it is not consulted.
        let root = Path::new("/darks/M31");
        assert_eq!(
            frame_type_for(Path::new("/darks/M31/a.fit"), root),
            FrameType::Light
        );
    }

    #[test]
    fn a_root_without_a_file_name_is_resolved_through_its_canonical_path() {
        let tmp = TempDir::new().unwrap();
        let darks = tmp.path().join("Darks");
        fs::create_dir_all(darks.join("sub")).unwrap();

        assert_eq!(folder_type(&darks), Some(FrameType::Dark));
        // `Darks/sub/..` ends in `..`, which has no file name of its own.
        assert_eq!(
            folder_type(&darks.join("sub").join("..")),
            Some(FrameType::Dark)
        );
        // The temp dir's own (random) name is not a type.
        assert_eq!(folder_type(&darks.join("..")), None);
    }

    #[test]
    fn night_cut_is_local_noon() {
        let t = |s: &str| parse_date_obs(s).unwrap();
        let night = |s: &str| night_of(t(s));

        // Same night across midnight.
        assert_eq!(night("2026-06-22T22:00:00"), night("2026-06-23T02:00:00"));
        assert_eq!(night("2026-06-22T12:00:00"), night("2026-06-23T11:59:59"));
        // 13:00 the next day starts the next night.
        assert_eq!(
            night("2026-06-23T13:00:00") - night("2026-06-22T22:00:00"),
            1
        );
        // Just before noon still belongs to the previous night.
        assert_eq!(
            night("2026-06-23T11:59:59") - night("2026-06-22T12:00:00"),
            0
        );
        assert_eq!(
            night("2026-06-23T12:00:00") - night("2026-06-22T12:00:00"),
            1
        );
    }

    #[test]
    fn civil_dates_round_trip_through_epoch_days() {
        for text in [
            "1969-12-31T00:00:00",
            "1970-01-01T00:00:00",
            "2000-02-29T00:00:00",
            "2024-12-31T00:00:00",
            "2026-06-22T00:00:00",
            "2100-03-01T00:00:00",
        ] {
            let epoch = parse_date_obs(text).unwrap();
            let (y, m, d) = civil_from_days((epoch / SECONDS_PER_DAY) as i64);
            assert_eq!(format!("{y:04}-{m:02}-{d:02}T00:00:00"), text);
        }
    }

    #[test]
    fn timestamps_and_durations_format_as_documented() {
        let epoch = parse_date_obs("2026-06-22T21:30:45").unwrap();
        assert_eq!(format_timestamp(epoch), "2026-06-22 21:30");
        let before_epoch = parse_date_obs("1969-12-31T23:59:00").unwrap();
        assert_eq!(format_timestamp(before_epoch), "1969-12-31 23:59");

        assert_eq!(format_duration(0.0), "0h 00m 00s");
        assert_eq!(format_duration(900.0), "0h 15m 00s");
        assert_eq!(
            format_duration(5.0 * 3600.0 + 12.0 * 60.0 + 30.0),
            "5h 12m 30s"
        );
        assert_eq!(format_duration(36_000.0), "10h 00m 00s");
        assert_eq!(format_duration(59.6), "0h 01m 00s");
        assert_eq!(format_duration(-3.0), "0h 00m 00s");
    }

    #[test]
    fn scan_finds_every_image_in_every_folder_with_its_type() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        frame(root, "LiGhTs/Ha/a.fit", None, 1.0, None, None);
        frame(root, "DARKS/b.fits", None, 1.0, None, None);
        frame(root, "Biases/c.fz", None, 1.0, None, None);
        frame(root, "misc/d.fit", None, 1.0, None, None); // no type folder
        frame(root, "e.fit", None, 1.0, None, None); // at the root
        fs::write(root.join("notes.txt"), b"x").unwrap();
        fs::write(root.join("lights.fit.bak"), b"x").unwrap();

        let found: Vec<(String, FrameType)> = scan_images(root)
            .into_iter()
            .map(|i| {
                let rel = i
                    .path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (rel, i.frame_type)
            })
            .collect();

        assert_eq!(
            found,
            vec![
                ("Biases/c.fz".to_string(), FrameType::Bias),
                ("DARKS/b.fits".to_string(), FrameType::Dark),
                ("LiGhTs/Ha/a.fit".to_string(), FrameType::Light),
                ("e.fit".to_string(), FrameType::Light),
                ("misc/d.fit".to_string(), FrameType::Light),
            ]
        );
    }

    #[test]
    fn light_count_covers_only_lights_across_sessions() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let at = |day: &str| format!("2026-06-{day}T22:00:00");
        frame(root, "Lights/a.fit", Some("L"), 60.0, Some(&at("22")), None);
        frame(root, "Lights/b.fit", Some("L"), 60.0, Some(&at("22")), None);
        frame(root, "Lights/c.fit", Some("L"), 60.0, Some(&at("23")), None);
        frame(root, "Darks/d.fit", None, 60.0, Some(&at("22")), None);

        let summary = summarize_dir(root).unwrap();

        assert_eq!(summary.sessions.len(), 2);
        assert_eq!(summary.count, 4);
        assert_eq!(summary.light_count(), 3);
    }

    #[test]
    fn filter_groups_without_a_filter_are_labelled() {
        let group = |filter: Option<&str>| FilterGroup::new(filter.map(str::to_string), Vec::new());
        assert_eq!(group(Some("Ha")).label(), "Ha");
        assert_eq!(group(None).label(), "(no filter)");
    }

    #[test]
    fn frame_types_have_singular_nouns() {
        assert_eq!(FrameType::Light.noun(), "light");
        assert_eq!(FrameType::DarkFlat.noun(), "dark flat");
    }

    #[test]
    fn non_finite_exposure_and_hostile_timestamps_are_ignored() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        frame(
            root,
            "inf.fit",
            None,
            f64::INFINITY,
            Some("99999999999999999-01-01T00:00:00"),
            Some("2026-06-22T22:00:00"),
        );

        let summary = summarize_dir(root).unwrap();

        // The unparseable DATE-LOC falls through to DATE-OBS; the infinite
        // exposure counts as zero rather than poisoning the totals.
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert_eq!(summary.count, 1);
        assert_eq!(summary.integration, 0.0);
        assert_eq!(names(&summary), ["2026-06-22"]);
    }

    #[test]
    fn summarizing_files_matches_summarizing_their_folder() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let at = |t: &str| format!("2026-06-22T{t}");
        let a = frame(
            root,
            "Lights/Ha/a.fit",
            Some("Ha"),
            300.0,
            Some(&at("22:00:00")),
            None,
        );
        let b = frame(
            root,
            "Lights/Ha/b.fit",
            Some("Ha"),
            300.0,
            Some(&at("22:10:00")),
            None,
        );
        let d = frame(
            root,
            "darks/d.fit",
            None,
            300.0,
            Some(&at("23:00:00")),
            None,
        );

        let by_dir = summarize_dir(root).unwrap();
        let by_files = summarize_files(&[d, a, b]);

        assert_eq!(by_files.root, root);
        assert_eq!((by_files.count, by_files.light_count()), (3, 2));
        assert_eq!(by_files.integration, by_dir.integration);
        assert_eq!(names(&by_files), names(&by_dir));
        let session = &by_files.sessions[0];
        assert_eq!(session.type_group(FrameType::Dark).unwrap().count, 1);
        assert_eq!(
            session.type_group(FrameType::Light).unwrap().filters[0].count,
            2
        );
    }

    #[test]
    fn a_files_own_folder_names_its_type_when_it_is_the_common_folder() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let a = frame(
            root,
            "Flats/a.fit",
            Some("L"),
            1.0,
            Some("2026-06-22T20:00:00"),
            None,
        );
        let b = frame(
            root,
            "Flats/sub/b.fit",
            Some("L"),
            1.0,
            Some("2026-06-22T20:01:00"),
            None,
        );

        // Both files sit under `Flats`, the deepest common folder.
        let summary = summarize_files(&[a.clone(), b]);
        assert_eq!(summary.root, root.join("Flats"));
        assert_eq!(summary.sessions[0].types[0].frame_type, FrameType::Flat);

        // A single file is its own folder's only content.
        let one = summarize_files(&[a]);
        assert_eq!(one.sessions[0].types[0].frame_type, FrameType::Flat);
    }

    #[test]
    fn summarizing_files_reports_unreadable_ones_and_handles_none() {
        let tmp = TempDir::new().unwrap();
        let good = frame(
            tmp.path(),
            "a.fit",
            None,
            10.0,
            Some("2026-06-22T20:00:00"),
            None,
        );
        let bad = tmp.path().join("broken.fit");
        fs::write(&bad, b"nope").unwrap();

        let summary = summarize_files(&[good, bad.clone()]);
        assert_eq!(summary.count, 1);
        assert_eq!(summary.errors.len(), 1);
        assert_eq!(summary.errors[0].0, bad);

        let empty = summarize_files(&[]);
        assert!(empty.sessions.is_empty() && empty.errors.is_empty());
    }

    #[test]
    fn common_dir_is_the_deepest_shared_folder() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(common_dir(&[p("/a/b/x.fit"), p("/a/b/c/y.fit")]), p("/a/b"));
        assert_eq!(common_dir(&[p("/a/x.fit"), p("/b/y.fit")]), p("/"));
        assert_eq!(common_dir(&[p("/a/x.fit")]), p("/a"));
        assert_eq!(common_dir(&[]), PathBuf::new());
        // `/a/bc` is not under `/a/b`.
        assert_eq!(common_dir(&[p("/a/b/x.fit"), p("/a/bc/y.fit")]), p("/a"));
    }

    #[test]
    fn a_missing_or_non_directory_root_is_an_error() {
        assert!(summarize_dir(Path::new("/no/such/dir")).is_err());
        let tmp = TempDir::new().unwrap();
        let file = frame(tmp.path(), "a.fit", None, 1.0, None, None);
        assert!(summarize_dir(&file).is_err());
    }

    #[test]
    fn an_empty_tree_has_no_sessions() {
        let tmp = TempDir::new().unwrap();
        let summary = summarize_dir(tmp.path()).unwrap();
        assert!(summary.sessions.is_empty());
        assert_eq!((summary.count, summary.integration), (0, 0.0));
        assert!(summary.errors.is_empty());
    }

    #[test]
    fn groups_by_night_type_and_filter_with_sums_at_every_level() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        // Night of 2026-06-22: lights through two filters, plus calibration.
        frame(
            r,
            "lights/a1.fit",
            Some("Ha"),
            300.0,
            Some("2026-06-22T22:00:00"),
            None,
        );
        frame(
            r,
            "lights/a2.fit",
            Some("Ha"),
            300.0,
            Some("2026-06-22T23:00:00"),
            None,
        );
        frame(
            r,
            "lights/b1.fit",
            Some("OIII"),
            120.0,
            Some("2026-06-23T01:30:00"),
            None,
        );
        frame(
            r,
            "darks/d1.fit",
            None,
            300.0,
            Some("2026-06-22T20:00:00"),
            None,
        );
        frame(
            r,
            "darks/d2.fit",
            None,
            300.0,
            Some("2026-06-22T20:05:00"),
            None,
        );
        frame(
            r,
            "flats/f1.fit",
            Some("Ha"),
            2.0,
            Some("2026-06-22T19:00:00"),
            None,
        );
        // Night of 2026-06-24.
        frame(
            r,
            "lights/c1.fit",
            Some("Ha"),
            600.0,
            Some("2026-06-24T21:00:00"),
            None,
        );

        let summary = summarize_dir(r).unwrap();

        assert_eq!(names(&summary), ["2026-06-22", "2026-06-24"]);
        assert_eq!(summary.count, 7);
        assert_eq!(summary.integration, 300.0 + 300.0 + 120.0 + 600.0);
        assert!(summary.errors.is_empty());

        let night = &summary.sessions[0];
        assert_eq!(night.count, 6);
        // Light-only integration: darks and flats are not counted.
        assert_eq!(night.integration, 720.0);
        let kinds: Vec<_> = night.types.iter().map(|t| t.frame_type).collect();
        assert_eq!(kinds, [FrameType::Light, FrameType::Dark, FrameType::Flat]);

        let lights = night.type_group(FrameType::Light).unwrap();
        assert_eq!((lights.count, lights.integration), (3, 720.0));
        let filters: Vec<_> = lights.filters.iter().map(|g| g.filter.as_deref()).collect();
        assert_eq!(filters, [Some("Ha"), Some("OIII")]);
        assert_eq!(
            (lights.filters[0].count, lights.filters[0].integration),
            (2, 600.0)
        );
        assert_eq!(
            (lights.filters[1].count, lights.filters[1].integration),
            (1, 120.0)
        );

        let darks = night.type_group(FrameType::Dark).unwrap();
        assert_eq!((darks.count, darks.integration), (2, 600.0));
        assert_eq!(darks.filters.len(), 1);
        assert_eq!(darks.filters[0].filter, None);

        let flats = night.type_group(FrameType::Flat).unwrap();
        assert_eq!((flats.count, flats.integration), (1, 2.0));

        // A session spans its earliest to its latest frame, any type.
        let at = |s: &str| parse_date_obs(s).unwrap();
        assert_eq!(night.start, Some(at("2026-06-22T19:00:00")));
        assert_eq!(night.end, Some(at("2026-06-23T01:30:00")));
        assert_eq!(night.frames().count(), 6);
    }

    #[test]
    fn images_outside_type_folders_default_to_light() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        frame(
            r,
            "M31/night1/x.fit",
            Some("L"),
            60.0,
            Some("2026-06-22T22:00:00"),
            None,
        );
        frame(
            r,
            "y.fit",
            Some("L"),
            60.0,
            Some("2026-06-22T22:10:00"),
            None,
        );

        let summary = summarize_dir(r).unwrap();

        assert_eq!(summary.sessions.len(), 1);
        let kinds: Vec<_> = summary.sessions[0]
            .types
            .iter()
            .map(|t| t.frame_type)
            .collect();
        assert_eq!(kinds, [FrameType::Light]);
        assert_eq!(summary.integration, 120.0);
    }

    #[test]
    fn a_type_folder_nested_in_unknown_folders_still_decides_the_type() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        frame(
            r,
            "2026-06-22/Darks/300s/x.fit",
            None,
            300.0,
            Some("2026-06-22T20:00:00"),
            None,
        );

        let summary = summarize_dir(r).unwrap();

        let session = &summary.sessions[0];
        assert!(session.type_group(FrameType::Light).is_none());
        assert_eq!(session.type_group(FrameType::Dark).unwrap().count, 1);
        assert_eq!(session.integration, 0.0);
    }

    #[test]
    fn a_night_crossing_midnight_is_one_session_and_the_afternoon_starts_the_next() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        frame(r, "a.fit", None, 10.0, Some("2026-06-22T22:00:00"), None);
        frame(r, "b.fit", None, 10.0, Some("2026-06-23T02:00:00"), None);
        frame(r, "c.fit", None, 10.0, Some("2026-06-23T13:00:00"), None);

        let summary = summarize_dir(r).unwrap();

        assert_eq!(names(&summary), ["2026-06-22", "2026-06-23"]);
        assert_eq!(summary.sessions[0].count, 2);
        assert_eq!(summary.sessions[1].count, 1);
    }

    #[test]
    fn frames_without_a_timestamp_form_a_last_unknown_date_session() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        frame(r, "a.fit", None, 10.0, None, None);
        frame(r, "b.fit", None, 10.0, Some("2026-06-22T22:00:00"), None);
        // A bare date does not parse as a timestamp.
        frame(r, "c.fit", None, 10.0, None, Some("2026-06-22"));

        let summary = summarize_dir(r).unwrap();

        assert_eq!(names(&summary), ["2026-06-22", "unknown date"]);
        let unknown = &summary.sessions[1];
        assert_eq!((unknown.night, unknown.count), (None, 2));
        assert_eq!((unknown.start, unknown.end), (None, None));
    }

    #[test]
    fn date_loc_is_preferred_over_date_obs() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        // UTC says the 23rd 03:00, local time says the night of the 22nd.
        frame(
            r,
            "a.fit",
            None,
            10.0,
            Some("2026-06-22T21:00:00"),
            Some("2026-06-23T03:00:00"),
        );
        // No DATE-LOC: DATE-OBS is used.
        frame(r, "b.fit", None, 10.0, None, Some("2026-06-25T22:00:00"));
        // An unparseable DATE-LOC falls back to DATE-OBS too.
        frame(
            r,
            "c.fit",
            None,
            10.0,
            Some("garbage"),
            Some("2026-06-27T22:00:00"),
        );

        let summary = summarize_dir(r).unwrap();

        assert_eq!(names(&summary), ["2026-06-22", "2026-06-25", "2026-06-27"]);
        let first = summary.sessions[0].start.unwrap();
        assert_eq!(first, parse_date_obs("2026-06-22T21:00:00").unwrap());
    }

    #[test]
    fn missing_exposure_counts_as_zero_and_blank_filter_as_no_filter() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        frame(
            r,
            "a.fit",
            Some("   "),
            0.0,
            Some("2026-06-22T22:00:00"),
            None,
        );
        frame(
            r,
            "b.fit",
            Some(" Ha "),
            30.0,
            Some("2026-06-22T22:01:00"),
            None,
        );

        let summary = summarize_dir(r).unwrap();

        let lights = summary.sessions[0].type_group(FrameType::Light).unwrap();
        let filters: Vec<_> = lights.filters.iter().map(|g| g.filter.as_deref()).collect();
        // Trimmed, and the filterless group last.
        assert_eq!(filters, [Some("Ha"), None]);
        assert_eq!(lights.integration, 30.0);
        assert_eq!(lights.count, 2);
    }

    #[test]
    fn filters_are_ordered_lrgb_then_ha_sii_oiii_then_the_rest_alphabetically() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        // Written in a scrambled order, with mixed case and unknown filters.
        let filters = [
            "OIII", "Zeta", "SII", "B", "alpha", "Ha", "G", "Duo-Band", "R", "L", "ha 7nm",
        ];
        for (i, filter) in filters.iter().enumerate() {
            frame(
                r,
                &format!("f{i}.fit"),
                Some(filter),
                1.0,
                Some("2026-06-22T22:00:00"),
                None,
            );
        }
        frame(r, "none.fit", None, 1.0, Some("2026-06-22T22:00:00"), None);

        let summary = summarize_dir(r).unwrap();

        let lights = summary.sessions[0].type_group(FrameType::Light).unwrap();
        let order: Vec<_> = lights.filters.iter().map(|g| g.filter.as_deref()).collect();
        assert_eq!(
            order,
            [
                Some("L"),
                Some("R"),
                Some("G"),
                Some("B"),
                Some("Ha"),
                Some("SII"),
                Some("OIII"),
                // Everything else, alphabetically ignoring case...
                Some("alpha"),
                Some("Duo-Band"),
                Some("ha 7nm"),
                Some("Zeta"),
                // ...and frames without a filter last.
                None,
            ]
        );
    }

    #[test]
    fn known_filter_names_match_ignoring_case() {
        assert!(filter_rank(Some("ha")) < filter_rank(Some("sii")));
        assert_eq!(filter_rank(Some("HA")), filter_rank(Some("Ha")));
        // Only the exact name is special: a longer name is just "another filter".
        assert_eq!(filter_rank(Some("Ha 7nm")), filter_rank(Some("Other")));
        assert!(filter_rank(Some("Other")) < filter_rank(None));
    }

    #[test]
    fn frames_are_ordered_by_time_then_path() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        frame(
            r,
            "z.fit",
            Some("L"),
            1.0,
            Some("2026-06-22T22:00:00"),
            None,
        );
        frame(
            r,
            "a.fit",
            Some("L"),
            1.0,
            Some("2026-06-22T23:00:00"),
            None,
        );
        frame(
            r,
            "m.fit",
            Some("L"),
            1.0,
            Some("2026-06-22T22:00:00"),
            None,
        );

        let summary = summarize_dir(r).unwrap();

        let group = &summary.sessions[0].types[0].filters[0];
        let order: Vec<_> = group
            .frames
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(order, ["m.fit", "z.fit", "a.fit"]);
    }

    #[test]
    fn an_unreadable_file_is_reported_and_the_rest_is_still_summarised() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        frame(
            r,
            "good.fit",
            Some("Ha"),
            60.0,
            Some("2026-06-22T22:00:00"),
            None,
        );
        let bad = r.join("bad.fit");
        fs::write(&bad, b"this is not a fits file").unwrap();

        let summary = summarize_dir(r).unwrap();

        assert_eq!(summary.count, 1);
        assert_eq!(summary.integration, 60.0);
        assert_eq!(summary.errors.len(), 1);
        assert_eq!(summary.errors[0].0, bad);
        assert!(!summary.errors[0].1.is_empty());
    }

    #[test]
    fn real_fits_files_group_by_their_own_filter_exposure_and_time() {
        let tmp = TempDir::new().unwrap();
        let lights = tmp.path().join("Lights");
        fs::create_dir(&lights).unwrap();
        fs::copy(test_data("uncompressed.fit"), lights.join("a.fit")).unwrap();
        fs::copy(test_data("bad-image/good1.fits"), lights.join("b.fits")).unwrap();

        let summary = summarize_dir(tmp.path()).unwrap();

        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert_eq!(summary.count, 2);
        let all: Vec<&Frame> = summary.sessions.iter().flat_map(|s| s.frames()).collect();
        assert!(all.iter().all(|f| f.exposure > 0.0 && f.observed.is_some()));
        let filters: Vec<String> = summary
            .sessions
            .iter()
            .flat_map(|s| s.types.iter())
            .flat_map(|t| t.filters.iter())
            .filter_map(|g| g.filter.clone())
            .collect();
        assert!(
            filters.contains(&"Narrowband 7nm".to_string()),
            "{filters:?}"
        );
        assert!(filters.contains(&"HaOIII 7nm".to_string()), "{filters:?}");
        let total: f64 = all.iter().map(|f| f.exposure).sum();
        assert_eq!(summary.integration, total);
    }

    #[test]
    fn a_real_raw_frame_is_grouped_without_a_filter() {
        let tmp = TempDir::new().unwrap();
        let darks = tmp.path().join("darks");
        fs::create_dir(&darks).unwrap();
        fs::copy(test_data("comet.ARW"), darks.join("IMG_0001.ARW")).unwrap();
        frame(
            tmp.path(),
            "lights/a.fit",
            Some("Ha"),
            60.0,
            Some("2026-06-22T22:00:00"),
            None,
        );

        let summary = summarize_dir(tmp.path()).unwrap();

        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        let raw = summary
            .sessions
            .iter()
            .find_map(|s| s.type_group(FrameType::Dark))
            .expect("the RAW frame is a dark");
        assert_eq!(raw.count, 1);
        assert_eq!(raw.filters[0].filter, None);
        // Exposure and time come from the EXIF-derived header.
        let frame = &raw.filters[0].frames[0];
        let header = load_header_from_file(&frame.path).unwrap();
        assert_eq!(Some(frame.exposure), header.get_float("EXPTIME"));
        assert_eq!(
            frame.observed,
            header.get_string("DATE-OBS").and_then(parse_date_obs)
        );
        assert!(frame.observed.is_some());
    }

    #[test]
    fn a_plain_tiff_is_skipped_by_the_scan() {
        let tmp = TempDir::new().unwrap();
        let r = tmp.path();
        frame(r, "a.fit", None, 10.0, Some("2026-06-22T22:00:00"), None);
        // Not a camera RAW: a processed export that happens to sit here.
        let tif = r.join("export.tif");
        let file = fs::File::create(&tif).unwrap();
        tiff::encoder::TiffEncoder::new(file)
            .unwrap()
            .write_image::<tiff::encoder::colortype::RGB8>(4, 4, &[0u8; 4 * 4 * 3])
            .unwrap();

        let summary = summarize_dir(r).unwrap();

        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert!(
            scan_images(r)
                .iter()
                .all(|i| !i.path.ends_with("export.tif"))
        );
        assert_eq!(summary.count, 1);
        assert!(
            summary
                .sessions
                .iter()
                .flat_map(|s| s.frames())
                .any(|f| f.path.ends_with("a.fit"))
        );
    }
}
