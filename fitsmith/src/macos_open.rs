//! macOS only: hooks `application:openURLs:` onto Winit's own
//! `NSApplicationDelegate`, so files opened via Finder (double-click /
//! "Open With") reach the working set.
//!
//! Finder delivers those opens as an Apple Event that AppKit turns into a
//! direct `-[delegate application:openURLs:]` call — not as `argv`, so
//! `std::env::args_os()` (used for `fitz`-CLI-style launches) is never
//! populated for them. Without a delegate implementing this method, AppKit
//! instead routes the event through `NSDocumentController` (see the method's
//! doc comment in `objc2-app-kit`), which fails because FitSmith declares no
//! `NSDocumentClass` — that's the "FitSmith cannot open files in the ...
//! format" alert.
//!
//! We can't just install our own `NSApplicationDelegate` in the usual way:
//! Winit registers its own on `NSApplication` and asserts that it stays the
//! one it registered (see `winit::platform_impl::macos::app_state`), so
//! replacing it wholesale panics deep inside Winit on the very next event.
//! Registering with `NSAppleEventManager` for the raw `odoc` Apple Event
//! doesn't work either — modern AppKit resolves `open`-triggered document
//! requests straight to the delegate method without ever routing through the
//! low-level Apple Event Manager dispatch table, so a handler registered
//! there is simply never called (confirmed empirically: it's silently
//! skipped, with no crash and no alert).
//!
//! Instead, we reach into Winit's already-installed delegate object and add
//! the method to *its* class at the Objective-C runtime level
//! (`class_addMethod`), leaving every method Winit itself defined untouched.
//! This is unusual but not exotic — it's the same technique other
//! Winit-based GUI toolkits (e.g. Tauri's `tao`) use to plug this exact gap.
//!
//! Must be installed after the Slint window exists, since that's what
//! creates `NSApplication` and Winit's delegate in the first place.

use std::cell::RefCell;
use std::path::PathBuf;

use objc2::ffi::class_addMethod;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::NSApplication;
use objc2_foundation::{NSArray, NSURL};
use slint::ComponentHandle;

use crate::{AppWindow, controller};

thread_local! {
    // The injected method has no ivars of its own to read (it's added to a
    // class we don't own), so the Slint window handle it needs lives here
    // instead. Main-thread only, matching where AppKit delivers the event.
    static OPEN_TARGET: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
}

/// Install the hook. Must be called after the Slint window is created (so
/// Winit's `NSApplicationDelegate` already exists).
pub fn install(app: &AppWindow) {
    OPEN_TARGET.with(|target| *target.borrow_mut() = Some(app.as_weak()));

    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let delegate = NSApplication::sharedApplication(mtm)
        .delegate()
        .expect("Winit installs an NSApplicationDelegate when the first window is created");
    let class = AsRef::<AnyObject>::as_ref(&delegate).class() as *const AnyClass as *mut AnyClass;

    // SAFETY: `application_open_urls` has the exact
    // `(id, SEL, NSApplication *, NSArray<NSURL *> *) -> void` signature this
    // Objective-C method requires.
    let imp: Imp = unsafe {
        std::mem::transmute::<
            unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject),
            Imp,
        >(application_open_urls)
    };
    // SAFETY: `class` is Winit's own delegate class, which doesn't already
    // implement `application:openURLs:`, so this only adds a method Winit
    // never defined rather than overriding one of its own.
    let added =
        unsafe { class_addMethod(class, sel!(application:openURLs:), imp, c"v@:@@".as_ptr()) };
    debug_assert!(added.as_bool(), "failed to hook application:openURLs:");
}

unsafe extern "C-unwind" fn application_open_urls(
    _this: *mut AnyObject,
    _cmd: Sel,
    _application: *mut AnyObject,
    urls: *mut AnyObject,
) {
    if urls.is_null() {
        return;
    }
    // SAFETY: AppKit calls this with the `NSArray<NSURL *> *` from the
    // matching Objective-C method signature.
    let urls = unsafe { &*urls.cast::<NSArray<NSURL>>() };
    let Some(app) = OPEN_TARGET
        .with(|target| target.borrow().clone())
        .and_then(|w| w.upgrade())
    else {
        return;
    };
    let paths: Vec<PathBuf> = urls
        .iter()
        .filter_map(|url| url.path())
        .map(|path| PathBuf::from(path.to_string()))
        .collect();
    if !paths.is_empty() {
        controller::open_args(&app, paths);
    }
}
