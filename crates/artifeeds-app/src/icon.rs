//! The application icon.
//!
//! A binary launched from a terminal is not a bundle, so macOS has no icon to
//! read from disk and shows the generic executable icon. Setting it at runtime
//! fixes the Dock, the ⌘-Tab switcher and the window's proxy icon; the icon
//! bytes are compiled into the binary so there is nothing to look up at start-up
//! and no way for it to go missing.
//!
//! `scripts/make_app_bundle.sh` additionally packs a real `.app`, which is what
//! Finder and the Launchpad read an icon from.

#[cfg(target_os = "macos")]
pub fn install() {
    use objc::runtime::{Class, Object};
    use objc::{class, msg_send, sel, sel_impl};

    type Id = *mut Object;

    // 512 px is plenty for the Dock and the switcher at any scale factor, and
    // keeps the binary small; the .app bundle carries the full .icns.
    const ICON_PNG: &[u8] =
        include_bytes!("../../../assets/AppIcon.iconset/icon_512x512.png");

    if Class::get("NSImage").is_none() {
        return;
    }
    unsafe {
        let data: Id = msg_send![
            class!(NSData),
            dataWithBytes: ICON_PNG.as_ptr()
            length: ICON_PNG.len()
        ];
        let image: Id = msg_send![class!(NSImage), alloc];
        let image: Id = msg_send![image, initWithData: data];
        if image.is_null() {
            tracing::warn!("could not decode the application icon");
            return;
        }
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setApplicationIconImage: image];
    }
}

/// Called once, after the platform application object exists.
#[cfg(target_os = "macos")]
pub fn install_after_launch() {
    install();
}

#[cfg(not(target_os = "macos"))]
pub fn install_after_launch() {}
