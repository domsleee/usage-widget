//! macOS renewal lookup in the widget's own sign-in window: a `WKWebView` on
//! claude.ai whose session lives in an identifier-keyed `WKWebsiteDataStore`
//! owned by the widget, so sign-in persists between lookups without reading
//! any browser's cookie store and without Node, Chrome or Playwright.

use crate::providers::Cycle;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSBackingStoreType, NSWindow, NSWindowStyleMask};
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
    NSURLRequest, NSUUID,
};
use objc2_web_kit::{
    WKNavigationAction, WKUIDelegate, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
    WKWindowFeatures,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

struct UiDelegateIvars {
    popup: RefCell<Option<Retained<NSWindow>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = UiDelegateIvars]
    struct UiDelegate;

    unsafe impl NSObjectProtocol for UiDelegate {}

    unsafe impl WKUIDelegate for UiDelegate {
        // OAuth sign-in (Continue with Google) opens a popup; without this the
        // window goes nowhere and the flow fails.
        #[unsafe(method_id(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        unsafe fn create_web_view(
            &self,
            _web_view: &WKWebView,
            configuration: &WKWebViewConfiguration,
            _action: &WKNavigationAction,
            _features: &WKWindowFeatures,
        ) -> Option<Retained<WKWebView>> {
            let mtm = MainThreadMarker::new().expect("UI delegate runs on the main thread");
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(480.0, 620.0));
            let popup = unsafe {
                WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, configuration)
            };
            unsafe { popup.setUIDelegate(Some(ProtocolObject::from_ref(self))) };
            let style = NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Resizable;
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    frame,
                    style,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&NSString::from_str("Sign in to Claude"));
            window.setContentView(Some(&popup));
            window.center();
            window.makeKeyAndOrderFront(None);
            if let Some(old) = self.ivars().popup.borrow_mut().replace(window) {
                old.close();
            }
            Some(popup)
        }

        #[unsafe(method(webViewDidClose:))]
        unsafe fn web_view_did_close(&self, web_view: &WKWebView) {
            let popup = self.ivars().popup.borrow();
            if let Some(window) = popup.as_ref()
                && window.contentView().map(|v| Retained::as_ptr(&v) as usize)
                    == Some(web_view as *const WKWebView as usize)
            {
                drop(popup);
                if let Some(window) = self.ivars().popup.borrow_mut().take() {
                    window.close();
                }
            }
        }
    }
);

impl UiDelegate {
    fn bring_popup_to_front(&self) {
        if let Some(window) = self.ivars().popup.borrow().as_ref() {
            window.makeKeyAndOrderFront(None);
        }
    }

    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let delegate = mtm.alloc::<Self>().set_ivars(UiDelegateIvars {
            popup: RefCell::new(None),
        });
        unsafe { msg_send![super(delegate), init] }
    }
}

/// Identifies the widget's claude.ai data store on disk. Fixed, so the same
/// store (and sign-in) is found on every run.
const STORE_ID: &str = "7E3A91C4-2B6D-4F58-A1C9-5D8E0B3F6A72";
const TIMEOUT: Duration = Duration::from_secs(450);
const POLL: Duration = Duration::from_millis(600);

type JsSlot = Rc<RefCell<Option<Option<String>>>>;

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    /// Waiting for the claude.ai workspace API to answer (sign-in).
    Orgs,
    /// Fetching subscription details for the chosen organization.
    Sub,
    Done,
}

pub struct WebLookup {
    window: Retained<NSWindow>,
    webview: Retained<WKWebView>,
    _ui_delegate: Retained<UiDelegate>,
    phase: Phase,
    org: Option<String>,
    expected_org: Option<String>,
    started: Instant,
    last_poll: Instant,
    js_started: bool,
    slot: JsSlot,
    _blocks: Vec<RcBlock<dyn Fn(*mut AnyObject, *mut objc2_foundation::NSError)>>,
    result: Option<Result<Cycle, String>>,
}

impl WebLookup {
    pub fn new() -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("Claude sign-in must run on the main thread")?;
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        let uuid = NSUUID::initWithUUIDString(NSUUID::alloc(), &NSString::from_str(STORE_ID))
            .ok_or("invalid renewal data store identifier")?;
        let store = unsafe { WKWebsiteDataStore::dataStoreForIdentifier(&uuid, mtm) };
        unsafe { config.setWebsiteDataStore(&store) };

        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(560.0, 680.0));
        let webview = unsafe {
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config)
        };
        let ui_delegate = UiDelegate::new(mtm);
        unsafe {
            webview.setUIDelegate(Some(ProtocolObject::from_ref(&*ui_delegate)));
        }
        let style =
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                frame,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Sign in to Claude"));
        window.setContentView(Some(&*webview));
        window.center();
        let url = NSURL::URLWithString(&NSString::from_str("https://claude.ai/"))
            .ok_or("invalid claude.ai URL")?;
        unsafe { webview.loadRequest(&NSURLRequest::requestWithURL(&url)) };
        window.makeKeyAndOrderFront(None);
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);

        Ok(Self {
            window,
            webview,
            _ui_delegate: ui_delegate,
            phase: Phase::Orgs,
            org: None,
            expected_org: crate::providers::claude_org(),
            started: Instant::now(),
            last_poll: Instant::now() - POLL,
            js_started: false,
            slot: Rc::new(RefCell::new(None)),
            _blocks: Vec::new(),
            result: None,
        })
    }

    /// Raises the sign-in window (and its OAuth popup, if open) above other
    /// windows, for when it has been lost behind them.
    pub fn bring_to_front(&self) {
        if let Some(mtm) = MainThreadMarker::new() {
            #[allow(deprecated)]
            NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }
        self.window.makeKeyAndOrderFront(None);
        self._ui_delegate.bring_popup_to_front();
    }

    /// Drives the state machine; called once per frame while running.
    pub fn try_recv(&mut self) -> Option<Result<Cycle, String>> {
        if let Some(result) = self.result.take() {
            return Some(result);
        }
        if self.phase != Phase::Done && !self.window.isVisible() {
            return Some(Err(
                "Claude sign-in window closed before the lookup finished".into(),
            ));
        }
        if self.started.elapsed() > TIMEOUT {
            return Some(Err("Renewal lookup timed out. Try again.".into()));
        }
        if self.last_poll.elapsed() < POLL {
            return None;
        }
        self.last_poll = Instant::now();
        match self.phase {
            Phase::Orgs => self.poll_orgs(),
            Phase::Sub => self.poll_sub(),
            Phase::Done => None,
        }
    }

    fn poll_orgs(&mut self) -> Option<Result<Cycle, String>> {
        if !self.js_started {
            self.eval(
                "window.__uw_orgs='__pending__';\
                 fetch('/api/organizations',{signal:AbortSignal.timeout(10000)})\
                 .then(r=>r.ok?r.json():null)\
                 .then(j=>{window.__uw_orgs=j?JSON.stringify(j):'__none__'})\
                 .catch(()=>{window.__uw_orgs='__none__'});'started'",
            );
            self.js_started = true;
            return None;
        }
        let value = self.read("window.__uw_orgs||'__unset__'")?;
        if value == "__unset__" {
            // The page reloaded (sign-in redirects), wiping the marker.
            self.js_started = false;
            return None;
        }
        if value == "__pending__" {
            return None;
        }
        self.js_started = false;
        if value == "__none__" {
            return None; // Not signed in yet; keep waiting.
        }
        let orgs: Vec<serde_json::Value> = serde_json::from_str(&value).ok()?;
        let uuids: Vec<&str> = orgs.iter().filter_map(|o| o["uuid"].as_str()).collect();
        let chosen = if let Some(expected) = &self.expected_org {
            if uuids.contains(&expected.as_str()) {
                Some(expected.clone())
            } else if !uuids.is_empty() {
                return Some(Err(
                    "Sign in to the same Claude account used by Claude Code".into(),
                ));
            } else {
                None
            }
        } else if uuids.len() == 1 {
            Some(uuids[0].to_string())
        } else if uuids.is_empty() {
            None
        } else {
            return Some(Err(
                "Select your personal workspace in Claude, then try again".into(),
            ));
        };
        if let Some(org) = chosen {
            self.org = Some(org);
            self.phase = Phase::Sub;
        }
        None
    }

    fn poll_sub(&mut self) -> Option<Result<Cycle, String>> {
        let org = self.org.clone()?;
        if !self.js_started {
            let org_js = serde_json::to_string(&org).ok()?;
            self.eval(&format!(
                "window.__uw_sub='__pending__';\
                 fetch('/api/organizations/'+encodeURIComponent({org_js})+'/subscription_details',\
                 {{signal:AbortSignal.timeout(25000)}})\
                 .then(async r=>{{window.__uw_sub=r.ok?JSON.stringify(await r.json()):'__http_'+r.status}})\
                 .catch(()=>{{window.__uw_sub='__error__'}});'started'"
            ));
            self.js_started = true;
            return None;
        }
        let value = self.read("window.__uw_sub||'__unset__'")?;
        if value == "__unset__" {
            self.js_started = false;
            return None;
        }
        if value == "__pending__" {
            return None;
        }
        self.phase = Phase::Done;
        self.window.close();
        if let Some(status) = value.strip_prefix("__http_") {
            return Some(Err(format!(
                "Claude subscription lookup returned HTTP {status}"
            )));
        }
        if value == "__error__" {
            return Some(Err("Claude subscription lookup failed. Try again.".into()));
        }
        let json: serde_json::Value = match serde_json::from_str(&value) {
            Ok(json) => json,
            Err(e) => return Some(Err(format!("bad JSON from claude.ai: {e}"))),
        };
        Some(crate::claude_web::cycle_from_details(&json))
    }

    /// Queues a script whose string result lands in the shared slot.
    fn eval(&mut self, js: &str) {
        *self.slot.borrow_mut() = None;
        let slot = self.slot.clone();
        let block = RcBlock::new(
            move |result: *mut AnyObject, _error: *mut objc2_foundation::NSError| {
                let text = unsafe { Retained::retain(result) }
                    .and_then(|obj| obj.downcast::<NSString>().ok())
                    .map(|s| s.to_string());
                *slot.borrow_mut() = Some(text);
            },
        );
        unsafe {
            self.webview
                .evaluateJavaScript_completionHandler(&NSString::from_str(js), Some(&block));
        }
        self._blocks.push(block);
        if self._blocks.len() > 8 {
            self._blocks.remove(0);
        }
    }

    /// Takes the last script result, if its completion has run.
    fn read(&mut self, js: &str) -> Option<String> {
        if let Some(done) = self.slot.borrow_mut().take() {
            return done;
        }
        self.eval(js);
        None
    }
}

impl Drop for WebLookup {
    fn drop(&mut self) {
        self.window.close();
    }
}
