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

/// An icon variant: its config id, the styled 1024 master (set as the Dock
/// icon), and a plain square thumbnail (for the settings radio, which rounds
/// it at render).
pub(crate) struct Icon {
    pub(crate) id: &'static str,
    pub(crate) master: &'static [u8],
    pub(crate) thumb: &'static [u8],
}

/// The variants, in display order for the settings picker (the default,
/// Son of Man, leads).
pub(crate) const ICONS: &[Icon] = &[
    Icon {
        id: "son-of-man",
        master: include_bytes!("../icons/son-of-man.png"),
        thumb: include_bytes!("../icons/thumb/son-of-man.png"),
    },
    Icon {
        id: "pipe",
        master: include_bytes!("../icons/pipe.png"),
        thumb: include_bytes!("../icons/thumb/pipe.png"),
    },
    Icon {
        id: "golconda",
        master: include_bytes!("../icons/golconda.png"),
        thumb: include_bytes!("../icons/thumb/golconda.png"),
    },
];

/// The variant used when `app_icon` is empty (or unrecognized). Kept in sync
/// with the bundle's Finder icon (see `packaging/macos/icons/make-icns.sh`).
pub(crate) const DEFAULT_ICON: &str = "son-of-man";

/// The effective icon id: the configured one, or the default when empty.
pub(crate) fn resolved_icon(configured: &str) -> &str {
    if configured.is_empty() {
        DEFAULT_ICON
    } else {
        configured
    }
}

/// The PNG bytes for a variant id, falling back to the default for an empty or
/// unknown id (so a typo or a future-removed variant still shows an icon).
pub(crate) fn icon_png(id: &str) -> &'static [u8] {
    let id = resolved_icon(id);
    ICONS
        .iter()
        .find(|icon| icon.id == id)
        .or_else(|| ICONS.iter().find(|icon| icon.id == DEFAULT_ICON))
        .expect("DEFAULT_ICON is a valid variant")
        .master
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

    /// Select an icon variant from the settings picker: persist it, set the
    /// running Dock icon, and save. Storing `DEFAULT_ICON` as empty keeps the
    /// config free of a redundant default entry.
    pub(crate) fn set_app_icon(&mut self, id: &str, cx: &mut gpui::Context<Self>) {
        let value = if id == DEFAULT_ICON {
            String::new()
        } else {
            id.to_string()
        };
        self.edit_global(|c| c.app_icon = value.clone());
        self.apply_app_icon();
        self.apply_and_save(cx);
    }
}
