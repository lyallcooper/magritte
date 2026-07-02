//! The app icon variant: which Magritte painting is the app's icon. macOS lets
//! us set the *running* Dock (and Cmd-Tab switcher) icon at runtime via
//! `NSApplication.applicationIconImage`; the bundle's Finder icon
//! (`CFBundleIconFile`) is fixed and can't be switched, so this only affects
//! the live Dock tile. The setting is reapplied on every launch (the override
//! is per-session) and whenever it changes.
//!
//! The variants embed the styled 1024px masters under `crates/magritte/icons/`
//! (see `packaging/macos/icons/make-icns.sh`), so the switcher works from the
//! binary alone — bundled or a plain `cargo run` — with no resource lookup.

/// The icon variants, as `(config id, display label, embedded master PNG)`.
/// The first is the default (used when `app_icon` is empty or unrecognized).
pub(crate) const ICONS: &[(&str, &str, &[u8])] = &[
    ("pipe", "Pipe", include_bytes!("../icons/pipe.png")),
    (
        "golconda",
        "Golconda",
        include_bytes!("../icons/golconda.png"),
    ),
    (
        "son-of-man",
        "Son of Man",
        include_bytes!("../icons/son-of-man.png"),
    ),
];

/// The PNG bytes for a variant id, falling back to the default for an empty or
/// unknown id (so a typo or a future-removed variant still shows an icon).
pub(crate) fn icon_png(id: &str) -> &'static [u8] {
    ICONS
        .iter()
        .find(|(vid, ..)| *vid == id)
        .unwrap_or(&ICONS[0])
        .2
}

/// Set the running app's Dock/switcher icon to `png`. macOS only; a no-op
/// elsewhere. Must be called on the main thread (AppKit requirement) — the
/// view's config/settings handlers already are.
#[cfg(target_os = "macos")]
pub(crate) fn set_dock_icon(png: &[u8]) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    type Id = *mut Object;

    // SAFETY: standard AppKit messages on the main thread. NSData copies the
    // bytes; NSImage/NSApplication are Apple singletons/owned objects. A failed
    // decode yields nil, which we check before setting.
    unsafe {
        let data: Id = msg_send![class!(NSData),
            dataWithBytes: png.as_ptr() as *const std::ffi::c_void
            length: png.len()];
        if data.is_null() {
            return;
        }
        let image: Id = msg_send![class!(NSImage), alloc];
        let image: Id = msg_send![image, initWithData: data];
        if image.is_null() {
            return;
        }
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn set_dock_icon(_png: &[u8]) {}

impl crate::StatusView {
    /// Apply the configured app icon to the running Dock tile. Called at launch
    /// and whenever the `app_icon` setting changes.
    pub(crate) fn apply_app_icon(&self) {
        set_dock_icon(icon_png(&self.config.app_icon));
    }
}
