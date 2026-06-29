//! Discovering installed GUI text editors (for the settings editor picker and
//! the "Open config in" menu). On macOS this asks LaunchServices which apps
//! register to *edit* text, minus a few over-claiming office suites.

use gpui::SharedString;

/// Label for the editor-picker entry that opens files in the OS default app
/// (an empty `editor` config).
pub(crate) const EDITOR_OS_DEFAULT_LABEL: &str = "System Default";

/// Installed text editors as (display name, `.app` path), for the settings
/// editor picker and the "Open config in" menu.
///
/// We ask LaunchServices which installed apps register to *edit* plain text or
/// source code (`kLSRolesEditor`) and union the two sets — that's macOS's own
/// notion of "apps that report as text editors", and it picks up whatever the
/// user actually has (VS Code, Zed, BBEdit, TextEdit, …) with no hand-kept
/// allow-list. The catch is that office suites and a few system apps over-claim
/// the editor role for plain text, so [`is_bogus_editor`] drops the known
/// offenders. Names/paths come from resolving each bundle id to its app URL.
#[cfg(target_os = "macos")]
pub(crate) fn text_editors() -> Vec<(SharedString, SharedString)> {
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use core_foundation::url::CFURL;
    use std::os::raw::c_void;

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSCopyAllRoleHandlersForContentType(content_type: CFStringRef, role: u32) -> CFArrayRef;
        fn LSCopyApplicationURLsForBundleIdentifier(
            bundle_id: CFStringRef,
            out_error: *mut c_void,
        ) -> CFArrayRef;
    }
    // kLSRolesEditor — handlers that can *edit* the type, not merely view it
    // (kLSRolesViewer, 0x2, which would pull in browsers and media players).
    const K_LS_ROLES_EDITOR: u32 = 0x0000_0004;

    let mut seen = std::collections::HashSet::new();
    let mut editors: Vec<(SharedString, SharedString)> = Vec::new();
    for content_type in ["public.plain-text", "public.source-code"] {
        let ct = CFString::new(content_type);
        let handlers = unsafe {
            let r =
                LSCopyAllRoleHandlersForContentType(ct.as_concrete_TypeRef(), K_LS_ROLES_EDITOR);
            if r.is_null() {
                continue;
            }
            CFArray::<CFString>::wrap_under_create_rule(r)
        };
        for bundle in handlers.iter() {
            let id = bundle.to_string();
            if is_bogus_editor(&id) || !seen.insert(id.clone()) {
                continue;
            }
            // Resolve the bundle id to its installed app URL(s); take the first.
            let urls = unsafe {
                let r = LSCopyApplicationURLsForBundleIdentifier(
                    bundle.as_concrete_TypeRef(),
                    std::ptr::null_mut(),
                );
                if r.is_null() {
                    continue;
                }
                CFArray::<CFURL>::wrap_under_create_rule(r)
            };
            if let Some(path) = urls.iter().next().and_then(|u| u.to_path()) {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| id.clone());
                editors.push((
                    SharedString::from(name),
                    SharedString::from(path.to_string_lossy().into_owned()),
                ));
            }
        }
    }
    editors.sort_by_key(|(name, _)| name.to_lowercase());
    editors
}

/// Bundle ids that register as plain-text editors but aren't general text
/// editors — office/productivity suites that over-claim the role.
#[cfg(target_os = "macos")]
fn is_bogus_editor(bundle_id: &str) -> bool {
    const DENY_PREFIXES: &[&str] = &[
        "com.apple.iWork",
        "com.apple.Numbers",
        "com.apple.Pages",
        "com.apple.Keynote",
        "com.apple.Notes",
        "com.microsoft.Word",
        "com.microsoft.Excel",
        "com.microsoft.Powerpoint",
        "org.libreoffice",
    ];
    DENY_PREFIXES.iter().any(|p| bundle_id.starts_with(p))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn text_editors() -> Vec<(SharedString, SharedString)> {
    Vec::new()
}
