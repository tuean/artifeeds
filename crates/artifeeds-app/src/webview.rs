//! An embedded `WKWebView`, used by reading mode to render the **original page**
//! with its own CSS, fonts, images and layout.
//!
//! Why a native view rather than re-rendering the HTML ourselves: the request is
//! to show the page *as it is*, and no amount of hand-rolled HTML/CSS handling
//! inside GPUI would reproduce a real site. So we attach a WebKit view as a
//! subview of GPUI's `NSView`, which means it moves, clips and resizes with the
//! window for free — no separate window to keep in sync.
//!
//! Division of labour:
//!
//! * GPUI keeps rendering the reading chrome (toolbar, and the distilled text
//!   behind the page as an instant, always-available fallback).
//! * The web view is only made visible once WebKit reports the navigation
//!   finished, so there is never a flash of a half-painted foreign page. If the
//!   load fails, the web view stays hidden and the text version is what you see.
//!
//! Everything here touches AppKit and must run on the main thread; GPUI renders
//! on the main thread, so the calls come from `App::render`.

use objc::declare::ClassDecl;
use objc::runtime::{BOOL, Class, NO, Object, Sel, YES};
use objc::{class, msg_send, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::ffi::{CStr, CString, c_char};
use std::sync::{Mutex, OnceLock};

#[link(name = "WebKit", kind = "framework")]
unsafe extern "C" {}

type Id = *mut Object;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

/// The last scroll fraction JavaScript reported, for the article on screen.
static SCROLL_PROGRESS: Mutex<f32> = Mutex::new(0.0);

/// The last cookie read, as a header value. `None` means "no answer yet"; an
/// empty string means the answer was "no cookies for that domain".
static COOKIE_JAR: Mutex<Option<String>> = Mutex::new(None);

/// Take the cookie jar, clearing it so the next request is a fresh answer.
pub fn take_cookies() -> Option<String> {
    COOKIE_JAR.lock().ok().and_then(|mut slot| slot.take())
}

/// Where the current navigation is. Read by the UI on the next frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    Idle,
    Loading,
    Loaded,
    Failed(String),
}

static STATE: Mutex<LoadState> = Mutex::new(LoadState::Idle);

pub fn state() -> LoadState {
    STATE.lock().map(|s| s.clone()).unwrap_or(LoadState::Idle)
}

fn set_state(next: LoadState) {
    if let Ok(mut slot) = STATE.lock() {
        *slot = next;
    }
}

/// The one WebKit view this process owns.
pub struct WebView {
    view: Id,
    parent: Id,
}

// Only ever touched from the main thread (GPUI's render).
unsafe impl Send for WebView {}
unsafe impl Sync for WebView {}

impl WebView {
    /// Attach a hidden `WKWebView` to the window's view. Safe to call once per
    /// window; returns `None` if WebKit is unavailable or the handle is not
    /// AppKit.
    pub fn attach(window: &gpui_kit::Window) -> Option<Self> {
        // `Window` has an inherent `window_handle()` returning an opaque
        // `AnyWindowHandle`; the raw AppKit view comes from the trait method.
        let handle = <gpui_kit::Window as HasWindowHandle>::window_handle(window).ok()?;
        let parent = match handle.as_raw() {
            RawWindowHandle::AppKit(appkit) => appkit.ns_view.as_ptr() as Id,
            other => {
                tracing::warn!("reading mode needs an AppKit window, got {other:?}");
                return None;
            }
        };
        if parent.is_null() || Class::get("WKWebView").is_none() {
            return None;
        }

        unsafe {
            // Default configuration: cookies, cache and JavaScript behave as
            // they do in Safari, which is what makes pages render "as they are".
            let config: Id = msg_send![class!(WKWebViewConfiguration), new];

            let alloc: Id = msg_send![webview_class(), alloc];
            let view: Id = msg_send![alloc, initWithFrame: CGRect::default() configuration: config];
            if view.is_null() {
                return None;
            }

            // A delegate is the only way to learn whether the page actually
            // arrived; without it a failed load is an indistinguishable blank
            // view. `navigationDelegate` is a *weak* property, so the object has
            // to be retained here or it is deallocated before the first callback
            // and the view stays hidden forever.
            let _: () = msg_send![view, setNavigationDelegate: retained_delegate()];

            // Hidden until the first successful navigation.
            let _: () = msg_send![view, setHidden: YES];
            let _: () = msg_send![parent, addSubview: view];

            Some(Self { view, parent })
        }
    }

    /// Fill the parent view below `top_inset` points. Honours AppKit's flipped
    /// coordinate system, so the toolbar gap is always at the top.
    pub fn layout(&self, top_inset: f64) {
        unsafe {
            let bounds: CGRect = msg_send![self.parent, bounds];
            let flipped: BOOL = msg_send![self.parent, isFlipped];
            let height = (bounds.size.height - top_inset).max(0.0);
            let y = if flipped == YES { top_inset } else { 0.0 };
            let frame = CGRect {
                origin: CGPoint { x: 0.0, y },
                size: CGSize { width: bounds.size.width, height },
            };
            let _: () = msg_send![self.view, setFrame: frame];
        }
    }

    pub fn set_visible(&self, visible: bool) {
        unsafe {
            let _: () = msg_send![self.view, setHidden: if visible { NO } else { YES }];
        }
    }

    /// Point the web view at `url`. The view stays hidden until the delegate
    /// reports success, so the reader can keep showing the distilled text in the
    /// meantime.
    pub fn load(&self, url: &str) {
        if url.is_empty() {
            set_state(LoadState::Idle);
            return;
        }
        unsafe {
            let nsurl: Id = msg_send![class!(NSURL), URLWithString: nsstring(url)];
            if nsurl.is_null() {
                set_state(LoadState::Failed("invalid URL".into()));
                return;
            }
            let request: Id = msg_send![class!(NSURLRequest), requestWithURL: nsurl];
            set_state(LoadState::Loading);
            let _: () = msg_send![self.view, setHidden: YES];
            let _: () = msg_send![self.view, loadRequest: request];
        }
    }

    /// Render an archived page straight from disk. `loadFileURL:` grants read
    /// access to the archive directory, which is all a self-contained file
    /// needs — the page makes no network requests of its own.
    pub fn load_file(&self, path: &str) {
        unsafe {
            let file = nsstring(&format!("file://{path}"));
            let nsurl: Id = msg_send![class!(NSURL), URLWithString: file];
            if nsurl.is_null() {
                set_state(LoadState::Failed("bad archive path".into()));
                return;
            }
            let parent: Id = msg_send![nsurl, URLByDeletingLastPathComponent];
            set_state(LoadState::Loading);
            let _: () = msg_send![self.view, setHidden: YES];
            let _: () = msg_send![self.view, loadFileURL: nsurl allowingReadAccessToURL: parent];
        }
    }

    pub fn reload(&self) {
        unsafe {
            set_state(LoadState::Loading);
            let _: Id = msg_send![self.view, reload];
        }
    }

    pub fn can_go_back(&self) -> bool {
        unsafe {
            let can: BOOL = msg_send![self.view, canGoBack];
            can == YES
        }
    }

    pub fn go_back(&self) {
        unsafe {
            let _: Id = msg_send![self.view, goBack];
        }
    }

    /// How far down the page the reader is, 0..1 — the last value JavaScript
    /// reported. `request_scroll_progress` is what refreshes it.
    ///
    /// There is no native shortcut: `WKWebView.scrollView` exists on iOS only
    /// (the header declares it inside `#if TARGET_OS_IPHONE`), and asking a
    /// macOS web view for it raises an `NSException` that aborts the process.
    /// JavaScript is the supported way to see the document's scroll state.
    pub fn scroll_progress(&self) -> f32 {
        *SCROLL_PROGRESS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ask the page where it is. Asynchronous — the answer lands in
    /// `scroll_progress()` a moment later, so callers poll rather than block.
    pub fn request_scroll_progress(&self) {
        const JS: &str = "(() => { const d = document.documentElement; \
             const span = d.scrollHeight - window.innerHeight; \
             return span > 4 ? Math.min(1, Math.max(0, window.scrollY / span)) : 1; })()";
        let handler = block::ConcreteBlock::new(move |value: Id, _error: Id| {
            let mut slot = SCROLL_PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
            // The result is an NSNumber; `doubleValue` is the conversion.
            let number: f64 = unsafe { msg_send![value, doubleValue] };
            *slot = number as f32;
        })
        .copy();
        unsafe {
            let _: () = msg_send![
                self.view,
                evaluateJavaScript: nsstring(JS)
                completionHandler: &*handler
            ];
        }
    }

    /// Read the cookies this web view holds for a domain, as a `Cookie:` header.
    ///
    /// This is how the Zhihu sign-in works: the reader logs in *here*, in the
    /// app's own browser, and the session is then read straight out of the app's
    /// web data store. Nothing is lifted from Safari or Edge, and the session
    /// never has to be pasted into a configuration file.
    ///
    /// `getAllCookies:` is asynchronous and hands its answer to a block, so the
    /// result lands in a slot the UI polls on a later frame — the same shape as
    /// the scroll fraction above.
    pub fn request_cookies(&self, domain: &str) {
        let wanted = domain.to_string();
        let handler = block::ConcreteBlock::new(move |cookies: Id| {
            let mut jar = String::new();
            unsafe {
                let count: usize = msg_send![cookies, count];
                for index in 0..count {
                    let cookie: Id = msg_send![cookies, objectAtIndex: index];
                    let name: Id = msg_send![cookie, name];
                    let value: Id = msg_send![cookie, value];
                    let cookie_domain: Id = msg_send![cookie, domain];
                    let (name, value, cookie_domain) = (
                        nsstring_to_rust(name).unwrap_or_default(),
                        nsstring_to_rust(value).unwrap_or_default(),
                        nsstring_to_rust(cookie_domain).unwrap_or_default(),
                    );
                    if !cookie_domain.contains(&wanted) || name.is_empty() {
                        continue;
                    }
                    if !jar.is_empty() {
                        jar.push_str("; ");
                    }
                    jar.push_str(&name);
                    jar.push('=');
                    jar.push_str(&value);
                }
            }
            if let Ok(mut slot) = COOKIE_JAR.lock() {
                *slot = Some(jar);
            }
        })
        .copy();
        unsafe {
            let store: Id = msg_send![self.view, configuration];
            let store: Id = msg_send![store, websiteDataStore];
            let http: Id = msg_send![store, httpCookieStore];
            let _: () = msg_send![http, getAllCookies: &*handler];
        }
    }

    /// Scroll the page by whole viewports, or to one end.
    ///
    /// The web view deliberately refuses first responder (so `j`, `b`, `d` keep
    /// working while an article is open), which also means it never sees the
    /// keyboard. Pages have to be scrolled with the trackpad otherwise, so the
    /// paging keys are translated into JavaScript here.
    pub fn scroll_page(&self, pages: f64) {
        let js = if pages == f64::NEG_INFINITY {
            "window.scrollTo(0, 0)".to_string()
        } else if pages == f64::INFINITY {
            "window.scrollTo(0, document.documentElement.scrollHeight)".to_string()
        } else {
            format!("window.scrollBy(0, window.innerHeight * {pages} * 0.9)")
        };
        unsafe {
            let _: () = msg_send![
                self.view,
                evaluateJavaScript: nsstring(&js)
                completionHandler: std::ptr::null::<Object>()
            ];
        }
    }

    /// Put the reader back where they were.
    pub fn set_scroll_progress(&self, fraction: f32) {
        let fraction = fraction.clamp(0.0, 1.0);
        let js = format!(
            "(() => {{ const d = document.documentElement; \
             const span = d.scrollHeight - window.innerHeight; \
             window.scrollTo(0, span > 0 ? span * {fraction} : 0); }})()"
        );
        unsafe {
            let _: () = msg_send![self.view, evaluateJavaScript: nsstring(&js) completionHandler: std::ptr::null::<Object>()];
        }
    }

    /// The URL the page is currently on. `-[WKWebView URL]` returns an
    /// `NSURL`, so it has to be turned into a string first — calling
    /// `UTF8String` on it directly raises an Objective-C exception.
    pub fn current_url(&self) -> Option<String> {
        unsafe {
            let url: Id = msg_send![self.view, URL];
            if url.is_null() {
                return None;
            }
            let absolute: Id = msg_send![url, absoluteString];
            nsstring_to_rust(absolute)
        }
    }
}

unsafe fn nsstring(text: &str) -> Id {
    let c = CString::new(text).unwrap_or_default();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
}

unsafe fn nsstring_to_rust(value: Id) -> Option<String> {
    if value.is_null() {
        return None;
    }
    unsafe {
        let ptr: *const c_char = msg_send![value, UTF8String];
        if ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

/// A `WKWebView` that refuses first responder status.
///
/// A web view normally takes the keyboard as soon as you click in it, which
/// would silently kill the application's own key handling — including `Esc`,
/// the documented way out of reading mode. Declining first responder keeps
/// mouse interaction intact (links are clicked, the wheel scrolls, because both
/// are dispatched by hit-testing rather than to the first responder) while every
/// keystroke continues to reach GPUI.
fn webview_class() -> &'static Class {
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    CLASS.get_or_init(|| {
        let mut decl = ClassDecl::new("ArtifeedsWebView", class!(WKWebView))
            .expect("web view subclass");
        unsafe {
            decl.add_method(
                sel!(acceptsFirstResponder),
                refuses_first_responder as extern "C" fn(&Object, Sel) -> BOOL,
            );
        }
        decl.register()
    })
}

extern "C" fn refuses_first_responder(_this: &Object, _cmd: Sel) -> BOOL {
    NO
}

/// The one navigation delegate, retained for the life of the process.
fn retained_delegate() -> Id {
    static DELEGATE: OnceLock<usize> = OnceLock::new();
    *DELEGATE.get_or_init(|| unsafe {
        let delegate: Id = msg_send![navigation_delegate_class(), new];
        delegate as usize
    }) as Id
}

fn navigation_delegate_class() -> &'static Class {
    static CLASS: OnceLock<&'static Class> = OnceLock::new();
    CLASS.get_or_init(|| {
        let superclass = class!(NSObject);
        let mut decl = ClassDecl::new("ArtifeedsNavigationDelegate", superclass)
            .expect("navigation delegate class");
        unsafe {
            decl.add_method(
                sel!(webView:didFinishNavigation:),
                did_finish as extern "C" fn(&Object, Sel, Id, Id),
            );
            decl.add_method(
                sel!(webView:didFailProvisionalNavigation:withError:),
                did_fail as extern "C" fn(&Object, Sel, Id, Id, Id),
            );
            decl.add_method(
                sel!(webView:didFailNavigation:withError:),
                did_fail as extern "C" fn(&Object, Sel, Id, Id, Id),
            );
        }
        decl.register()
    })
}

extern "C" fn did_finish(_this: &Object, _cmd: Sel, _web_view: Id, _navigation: Id) {
    set_state(LoadState::Loaded);
}

extern "C" fn did_fail(_this: &Object, _cmd: Sel, _web_view: Id, _navigation: Id, error: Id) {
    let text = unsafe {
        let description: Id = msg_send![error, localizedDescription];
        nsstring_to_rust(description).unwrap_or_else(|| "load failed".to_string())
    };
    set_state(LoadState::Failed(text));
}

