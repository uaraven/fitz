//! The crop rectangle's editing rules, free of Slint and of threading: press /
//! drag / release gestures on a rectangle in *image pixel* coordinates, with an
//! optional locked aspect ratio. The UI only forwards pointer events here and
//! draws whatever [`CropEditor::rect`] says.
//!
//! Gestures, as the Crop tool specifies them:
//! - drag a side or corner handle: resize;
//! - drag inside the rectangle: move it;
//! - drag outside it: discard it and start a new one (a plain click outside
//!   just discards it).

use libfitz::crop::CropRect;

/// Smallest side, in image pixels, a finished rectangle may have.
const MIN_SIZE: f32 = 8.0;

/// Mouse-cursor codes understood by the window's `crop-cursor` property.
pub mod cursor {
    pub const CROSSHAIR: i32 = 1;
    pub const MOVE: i32 = 2;
    pub const NS_RESIZE: i32 = 3;
    pub const EW_RESIZE: i32 = 4;
    pub const NWSE_RESIZE: i32 = 5;
    pub const NESW_RESIZE: i32 = 6;
}

/// Aspect ratios of the toolbar's dropdown, in its order: index 0 is free-form,
/// the rest are `width:height` (each landscape ratio followed by its inverse).
const ASPECTS: [Option<(f32, f32)>; 12] = [
    None,
    Some((1.0, 1.0)),
    Some((3.0, 2.0)),
    Some((2.0, 3.0)),
    Some((4.0, 3.0)),
    Some((3.0, 4.0)),
    Some((5.0, 4.0)),
    Some((4.0, 5.0)),
    Some((16.0, 9.0)),
    Some((9.0, 16.0)),
    Some((16.0, 10.0)),
    Some((10.0, 16.0)),
];

/// The width/height ratio for a dropdown index (`None` = free-form, also for an
/// out-of-range index).
pub fn aspect_for_index(index: i32) -> Option<f32> {
    usize::try_from(index)
        .ok()
        .and_then(|i| ASPECTS.get(i).copied().flatten())
        .map(|(w, h)| w / h)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Handle {
    fn cursor(self) -> i32 {
        match self {
            Handle::N | Handle::S => cursor::NS_RESIZE,
            Handle::E | Handle::W => cursor::EW_RESIZE,
            Handle::NW | Handle::SE => cursor::NWSE_RESIZE,
            Handle::NE | Handle::SW => cursor::NESW_RESIZE,
        }
    }
}

/// What lies under a point, relative to the rectangle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Handle(Handle),
    Inside,
    Outside,
}

impl Hit {
    pub fn cursor(self) -> i32 {
        match self {
            Hit::Handle(h) => h.cursor(),
            Hit::Inside => cursor::MOVE,
            Hit::Outside => cursor::CROSSHAIR,
        }
    }
}

type Point = (f32, f32);
type Bounds = (f32, f32);

enum DragKind {
    /// A corner handle: the opposite corner stays put.
    Corner(Point),
    /// A side handle.
    Edge(Handle),
    /// Moving: the press point's offset from the rectangle's origin.
    Move(Point),
    /// A fresh rectangle growing from where the press landed.
    New(Point),
}

struct Drag {
    kind: DragKind,
    /// The rectangle as it was at the press, restored if a resize ends too small.
    start: Option<Rect>,
    cursor: i32,
}

/// The crop rectangle and the gesture in progress on it.
#[derive(Default)]
pub struct CropEditor {
    rect: Option<Rect>,
    /// Width / height to hold the rectangle to; `None` is free-form.
    aspect: Option<f32>,
    drag: Option<Drag>,
}

impl CropEditor {
    pub fn rect(&self) -> Option<Rect> {
        self.rect
    }

    pub fn clear(&mut self) {
        self.rect = None;
        self.drag = None;
    }

    /// The rectangle in whole image pixels, ready for `libfitz::crop`.
    pub fn crop_rect(&self) -> Option<CropRect> {
        let r = self.rect?;
        let width = r.w.round() as usize;
        let height = r.h.round() as usize;
        (width > 0 && height > 0).then(|| CropRect {
            x: r.x.round().max(0.0) as usize,
            y: r.y.round().max(0.0) as usize,
            width,
            height,
        })
    }

    /// Hold the rectangle to `aspect` (width / height; `None` = free-form),
    /// refitting an existing rectangle around its centre.
    pub fn set_aspect(&mut self, aspect: Option<f32>, bounds: Bounds) {
        self.aspect = aspect;
        if let Some(r) = self.rect {
            self.rect =
                Some(self.fit_around_centre(r.w, r.h, (r.x + r.w / 2.0, r.y + r.h / 2.0), bounds));
        }
    }

    /// Start with a centred rectangle covering 80% of the frame, unless there
    /// already is one.
    pub fn seed_default(&mut self, bounds: Bounds) {
        if self.rect.is_none() && bounds.0 > 0.0 && bounds.1 > 0.0 {
            self.rect = Some(self.fit_around_centre(
                bounds.0 * 0.8,
                bounds.1 * 0.8,
                (bounds.0 / 2.0, bounds.1 / 2.0),
                bounds,
            ));
        }
    }

    /// A `w` x `h` box shrunk to the aspect ratio and the frame, centred on
    /// `centre` and nudged back inside the frame.
    fn fit_around_centre(&self, mut w: f32, mut h: f32, centre: Point, (bw, bh): Bounds) -> Rect {
        if let Some(a) = self.aspect {
            if w / h > a {
                w = h * a;
            } else {
                h = w / a;
            }
            if w > bw {
                w = bw;
                h = w / a;
            }
            if h > bh {
                h = bh;
                w = h * a;
            }
        }
        w = w.min(bw);
        h = h.min(bh);
        Rect {
            x: (centre.0 - w / 2.0).clamp(0.0, bw - w),
            y: (centre.1 - h / 2.0).clamp(0.0, bh - h),
            w,
            h,
        }
    }

    /// What lies under `p`; `tol` is the handle grab distance in image pixels.
    pub fn hit(&self, p: Point, tol: f32) -> Hit {
        let Some(r) = self.rect else {
            return Hit::Outside;
        };
        // A small rectangle must keep an interior to grab for moving.
        let tx = tol.min(r.w / 3.0);
        let ty = tol.min(r.h / 3.0);
        let in_x = p.0 >= r.x - tx && p.0 <= r.x + r.w + tx;
        let in_y = p.1 >= r.y - ty && p.1 <= r.y + r.h + ty;
        if !(in_x && in_y) {
            return Hit::Outside;
        }
        let left = (p.0 - r.x).abs() <= tx;
        let right = (p.0 - (r.x + r.w)).abs() <= tx;
        let top = (p.1 - r.y).abs() <= ty;
        let bottom = (p.1 - (r.y + r.h)).abs() <= ty;
        let handle = match (left, right, top, bottom) {
            (true, _, true, _) => Handle::NW,
            (_, true, true, _) => Handle::NE,
            (true, _, _, true) => Handle::SW,
            (_, true, _, true) => Handle::SE,
            (_, _, true, _) => Handle::N,
            (_, _, _, true) => Handle::S,
            (true, ..) => Handle::W,
            (_, true, ..) => Handle::E,
            _ => return Hit::Inside,
        };
        Hit::Handle(handle)
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The cursor to show at `p` — the active gesture's while one is running.
    pub fn cursor_at(&self, p: Point, tol: f32) -> i32 {
        match &self.drag {
            Some(d) => d.cursor,
            None => self.hit(p, tol).cursor(),
        }
    }

    /// Begin the gesture the point `p` calls for. Returns the cursor to show.
    pub fn press(&mut self, p: Point, tol: f32, (bw, bh): Bounds) -> i32 {
        let hit = self.hit(p, tol);
        let start = self.rect;
        let kind = match (hit, self.rect) {
            (Hit::Handle(h), Some(r)) => match h {
                Handle::NW => DragKind::Corner((r.x + r.w, r.y + r.h)),
                Handle::NE => DragKind::Corner((r.x, r.y + r.h)),
                Handle::SW => DragKind::Corner((r.x + r.w, r.y)),
                Handle::SE => DragKind::Corner((r.x, r.y)),
                edge => DragKind::Edge(edge),
            },
            (Hit::Inside, Some(r)) => DragKind::Move((p.0 - r.x, p.1 - r.y)),
            _ => {
                self.rect = None;
                DragKind::New((p.0.clamp(0.0, bw), p.1.clamp(0.0, bh)))
            }
        };
        let cursor = if matches!(kind, DragKind::New(_)) {
            cursor::CROSSHAIR
        } else {
            hit.cursor()
        };
        self.drag = Some(Drag {
            kind,
            start,
            cursor,
        });
        cursor
    }

    /// Continue the running gesture with the pointer now at `p`.
    pub fn drag_to(&mut self, p: Point, bounds: Bounds) {
        let Some(drag) = &self.drag else { return };
        let (bw, bh) = bounds;
        match (&drag.kind, drag.start) {
            (DragKind::Corner(anchor) | DragKind::New(anchor), _) => {
                self.rect = Some(rect_from_corner(*anchor, p, self.aspect, bounds));
            }
            (DragKind::Edge(handle), Some(start)) => {
                self.rect = Some(resize_edge(*handle, start, p, self.aspect, bounds));
            }
            (DragKind::Move((dx, dy)), Some(start)) => {
                self.rect = Some(Rect {
                    x: (p.0 - dx).clamp(0.0, (bw - start.w).max(0.0)),
                    y: (p.1 - dy).clamp(0.0, (bh - start.h).max(0.0)),
                    ..start
                });
            }
            _ => {}
        }
    }

    /// End the gesture. A rectangle left smaller than the minimum is dropped
    /// (a new one) or put back as it was (a resize).
    pub fn release(&mut self) {
        let Some(drag) = self.drag.take() else { return };
        let too_small = self.rect.is_some_and(|r| r.w < MIN_SIZE || r.h < MIN_SIZE);
        if too_small {
            self.rect = match drag.kind {
                DragKind::New(_) => None,
                _ => drag.start,
            };
        }
    }
}

/// The rectangle spanned by a fixed `anchor` corner and the pointer at `p`,
/// kept inside the frame and, with an aspect ratio, as large as the pointer
/// asks for without leaving it.
fn rect_from_corner(anchor: Point, p: Point, aspect: Option<f32>, (bw, bh): Bounds) -> Rect {
    let p = (p.0.clamp(0.0, bw), p.1.clamp(0.0, bh));
    let right = p.0 >= anchor.0;
    let down = p.1 >= anchor.1;
    let room_x = if right { bw - anchor.0 } else { anchor.0 };
    let room_y = if down { bh - anchor.1 } else { anchor.1 };
    let mut w = (p.0 - anchor.0).abs();
    let mut h = (p.1 - anchor.1).abs();
    if let Some(a) = aspect {
        w = w.max(h * a).min(room_x).min(room_y * a);
        h = w / a;
    }
    Rect {
        x: if right { anchor.0 } else { anchor.0 - w },
        y: if down { anchor.1 } else { anchor.1 - h },
        w,
        h,
    }
}

/// `start` with one side dragged to the pointer at `p`. With an aspect ratio
/// the other dimension follows, centred on `start`'s middle.
fn resize_edge(
    handle: Handle,
    start: Rect,
    p: Point,
    aspect: Option<f32>,
    (bw, bh): Bounds,
) -> Rect {
    let mut r = start;
    let (cx, cy) = (start.x + start.w / 2.0, start.y + start.h / 2.0);
    match handle {
        Handle::E | Handle::W => {
            if handle == Handle::E {
                r.w = p.0.clamp(start.x + MIN_SIZE, bw) - start.x;
            } else {
                let left = p.0.clamp(0.0, start.x + start.w - MIN_SIZE);
                r.x = left;
                r.w = start.x + start.w - left;
            }
            if let Some(a) = aspect {
                r.w = r.w.min(2.0 * cy.min(bh - cy) * a);
                r.h = r.w / a;
                r.y = cy - r.h / 2.0;
                if handle == Handle::W {
                    r.x = start.x + start.w - r.w;
                }
            }
        }
        _ => {
            if handle == Handle::S {
                r.h = p.1.clamp(start.y + MIN_SIZE, bh) - start.y;
            } else {
                let top = p.1.clamp(0.0, start.y + start.h - MIN_SIZE);
                r.y = top;
                r.h = start.y + start.h - top;
            }
            if let Some(a) = aspect {
                r.h = r.h.min(2.0 * cx.min(bw - cx) / a);
                r.w = r.h * a;
                r.x = cx - r.w / 2.0;
                if handle == Handle::N {
                    r.y = start.y + start.h - r.h;
                }
            }
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: Bounds = (1000.0, 800.0);

    fn editor_with(x: f32, y: f32, w: f32, h: f32) -> CropEditor {
        CropEditor {
            rect: Some(Rect { x, y, w, h }),
            ..CropEditor::default()
        }
    }

    fn approx(a: Rect, b: Rect) {
        for (l, r) in [(a.x, b.x), (a.y, b.y), (a.w, b.w), (a.h, b.h)] {
            assert!((l - r).abs() < 1e-3, "{a:?} != {b:?}");
        }
    }

    #[test]
    fn aspect_dropdown_maps_to_ratios() {
        assert_eq!(aspect_for_index(0), None);
        assert_eq!(aspect_for_index(1), Some(1.0));
        assert_eq!(aspect_for_index(2), Some(1.5));
        assert_eq!(aspect_for_index(3), Some(2.0 / 3.0));
        assert_eq!(aspect_for_index(10), Some(1.6));
        assert_eq!(aspect_for_index(99), None);
        assert_eq!(aspect_for_index(-1), None);
    }

    #[test]
    fn hit_distinguishes_handles_inside_and_outside() {
        let e = editor_with(100.0, 100.0, 400.0, 300.0);
        assert_eq!(e.hit((102.0, 98.0), 6.0), Hit::Handle(Handle::NW));
        assert_eq!(e.hit((498.0, 101.0), 6.0), Hit::Handle(Handle::NE));
        assert_eq!(e.hit((100.0, 401.0), 6.0), Hit::Handle(Handle::SW));
        assert_eq!(e.hit((503.0, 399.0), 6.0), Hit::Handle(Handle::SE));
        assert_eq!(e.hit((300.0, 97.0), 6.0), Hit::Handle(Handle::N));
        assert_eq!(e.hit((300.0, 402.0), 6.0), Hit::Handle(Handle::S));
        assert_eq!(e.hit((96.0, 250.0), 6.0), Hit::Handle(Handle::W));
        assert_eq!(e.hit((504.0, 250.0), 6.0), Hit::Handle(Handle::E));
        assert_eq!(e.hit((300.0, 250.0), 6.0), Hit::Inside);
        assert_eq!(e.hit((50.0, 50.0), 6.0), Hit::Outside);
        assert_eq!(e.hit((300.0, 420.0), 6.0), Hit::Outside);
        assert_eq!(CropEditor::default().hit((1.0, 1.0), 6.0), Hit::Outside);
    }

    #[test]
    fn a_small_rectangle_keeps_an_interior_to_grab() {
        let e = editor_with(100.0, 100.0, 12.0, 12.0);
        assert_eq!(e.hit((106.0, 106.0), 10.0), Hit::Inside);
    }

    #[test]
    fn dragging_a_corner_resizes_around_the_opposite_corner() {
        let mut e = editor_with(100.0, 100.0, 400.0, 300.0);
        e.press((500.0, 400.0), 6.0, BOUNDS);
        e.drag_to((600.0, 450.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 100.0,
                y: 100.0,
                w: 500.0,
                h: 350.0,
            },
        );

        // Dragging past the anchor flips the rectangle rather than collapsing it.
        e.press((600.0, 450.0), 6.0, BOUNDS);
        e.drag_to((50.0, 60.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 50.0,
                y: 60.0,
                w: 50.0,
                h: 40.0,
            },
        );
    }

    #[test]
    fn dragging_a_side_moves_only_that_side() {
        let mut e = editor_with(100.0, 100.0, 400.0, 300.0);
        e.press((500.0, 250.0), 6.0, BOUNDS);
        e.drag_to((700.0, 300.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 100.0,
                y: 100.0,
                w: 600.0,
                h: 300.0,
            },
        );

        e.press((100.0, 250.0), 6.0, BOUNDS);
        e.drag_to((40.0, 250.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 40.0,
                y: 100.0,
                w: 660.0,
                h: 300.0,
            },
        );

        e.press((300.0, 100.0), 6.0, BOUNDS);
        e.drag_to((300.0, 150.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 40.0,
                y: 150.0,
                w: 660.0,
                h: 250.0,
            },
        );

        e.press((300.0, 400.0), 6.0, BOUNDS);
        e.drag_to((300.0, 900.0), BOUNDS);
        e.release();
        // Clamped to the frame.
        approx(
            e.rect().unwrap(),
            Rect {
                x: 40.0,
                y: 150.0,
                w: 660.0,
                h: 650.0,
            },
        );
    }

    #[test]
    fn dragging_inside_moves_and_stays_in_the_frame() {
        let mut e = editor_with(100.0, 100.0, 400.0, 300.0);
        e.press((300.0, 250.0), 6.0, BOUNDS);
        e.drag_to((350.0, 270.0), BOUNDS);
        approx(
            e.rect().unwrap(),
            Rect {
                x: 150.0,
                y: 120.0,
                w: 400.0,
                h: 300.0,
            },
        );
        e.drag_to((5000.0, -5000.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 600.0,
                y: 0.0,
                w: 400.0,
                h: 300.0,
            },
        );
    }

    #[test]
    fn dragging_outside_starts_a_new_rectangle() {
        let mut e = editor_with(100.0, 100.0, 200.0, 200.0);
        e.press((600.0, 500.0), 6.0, BOUNDS);
        assert!(e.rect().is_none(), "the old rectangle is dismissed at once");
        e.drag_to((700.0, 650.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 600.0,
                y: 500.0,
                w: 100.0,
                h: 150.0,
            },
        );
    }

    #[test]
    fn a_plain_click_outside_dismisses_the_rectangle() {
        let mut e = editor_with(100.0, 100.0, 200.0, 200.0);
        e.press((600.0, 500.0), 6.0, BOUNDS);
        e.release();
        assert!(e.rect().is_none());
        assert!(e.crop_rect().is_none());
    }

    #[test]
    fn a_resize_that_ends_too_small_is_undone() {
        let mut e = editor_with(100.0, 100.0, 200.0, 200.0);
        e.press((300.0, 300.0), 6.0, BOUNDS);
        e.drag_to((102.0, 102.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 100.0,
                y: 100.0,
                w: 200.0,
                h: 200.0,
            },
        );
    }

    #[test]
    fn a_locked_aspect_holds_through_corner_and_edge_drags() {
        let mut e = CropEditor::default();
        e.set_aspect(Some(2.0), BOUNDS);
        e.press((100.0, 100.0), 6.0, BOUNDS);
        e.drag_to((500.0, 160.0), BOUNDS);
        e.release();
        let r = e.rect().unwrap();
        approx(
            r,
            Rect {
                x: 100.0,
                y: 100.0,
                w: 400.0,
                h: 200.0,
            },
        );

        // An edge drag keeps the ratio, growing the other side about the centre.
        e.press((500.0, 200.0), 6.0, BOUNDS);
        e.drag_to((700.0, 200.0), BOUNDS);
        e.release();
        approx(
            e.rect().unwrap(),
            Rect {
                x: 100.0,
                y: 50.0,
                w: 600.0,
                h: 300.0,
            },
        );
    }

    #[test]
    fn a_locked_aspect_never_leaves_the_frame() {
        let mut e = CropEditor::default();
        e.set_aspect(Some(1.0), BOUNDS);
        e.press((900.0, 700.0), 6.0, BOUNDS);
        e.drag_to((5000.0, 5000.0), BOUNDS);
        e.release();
        let r = e.rect().unwrap();
        assert!(r.x >= 0.0 && r.y >= 0.0);
        assert!(r.x + r.w <= BOUNDS.0 + 1e-3 && r.y + r.h <= BOUNDS.1 + 1e-3);
        assert!((r.w - r.h).abs() < 1e-3);
        approx(
            r,
            Rect {
                x: 900.0,
                y: 700.0,
                w: 100.0,
                h: 100.0,
            },
        );
    }

    #[test]
    fn changing_the_aspect_refits_the_rectangle_around_its_centre() {
        let mut e = editor_with(100.0, 100.0, 400.0, 300.0);
        e.set_aspect(Some(1.0), BOUNDS);
        approx(
            e.rect().unwrap(),
            Rect {
                x: 150.0,
                y: 100.0,
                w: 300.0,
                h: 300.0,
            },
        );
        e.set_aspect(None, BOUNDS);
        approx(
            e.rect().unwrap(),
            Rect {
                x: 150.0,
                y: 100.0,
                w: 300.0,
                h: 300.0,
            },
        );
    }

    #[test]
    fn seeding_covers_most_of_the_frame_and_respects_the_aspect() {
        let mut e = CropEditor::default();
        e.seed_default(BOUNDS);
        approx(
            e.rect().unwrap(),
            Rect {
                x: 100.0,
                y: 80.0,
                w: 800.0,
                h: 640.0,
            },
        );

        let mut sq = CropEditor::default();
        sq.set_aspect(Some(1.0), BOUNDS);
        sq.seed_default(BOUNDS);
        approx(
            sq.rect().unwrap(),
            Rect {
                x: 180.0,
                y: 80.0,
                w: 640.0,
                h: 640.0,
            },
        );

        // An existing rectangle is left alone, and an unknown frame seeds nothing.
        let before = e.rect();
        e.seed_default((10.0, 10.0));
        assert_eq!(e.rect(), before);
        let mut none = CropEditor::default();
        none.seed_default((0.0, 0.0));
        assert!(none.rect().is_none());
    }

    #[test]
    fn crop_rect_rounds_to_whole_pixels() {
        let e = editor_with(10.4, 20.6, 99.5, 50.2);
        assert_eq!(
            e.crop_rect(),
            Some(CropRect {
                x: 10,
                y: 21,
                width: 100,
                height: 50
            })
        );
    }

    #[test]
    fn the_cursor_follows_the_gesture_in_progress() {
        let mut e = editor_with(100.0, 100.0, 400.0, 300.0);
        assert_eq!(e.cursor_at((300.0, 250.0), 6.0), cursor::MOVE);
        assert_eq!(e.cursor_at((500.0, 400.0), 6.0), cursor::NWSE_RESIZE);
        assert_eq!(e.cursor_at((10.0, 10.0), 6.0), cursor::CROSSHAIR);
        e.press((500.0, 400.0), 6.0, BOUNDS);
        // Mid-drag the pointer is over the interior, but the cursor stays a resize one.
        assert_eq!(e.cursor_at((300.0, 250.0), 6.0), cursor::NWSE_RESIZE);
        e.release();
        assert_eq!(e.cursor_at((300.0, 250.0), 6.0), cursor::MOVE);
    }
}
