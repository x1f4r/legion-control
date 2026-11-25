//! Windows WebView2 process-failure hook isolated behind a safe API so `legion-app` keeps
//! `#![forbid(unsafe_code)]`.

/// Registers a callback on WebView2's `ProcessFailed` event so the native shell can release
/// display-awake holds and full-screen state if the browser, renderer, or GPU process terminates
/// or hangs.
#[cfg(all(windows, feature = "webview2"))]
#[allow(unsafe_code)]
pub fn register_process_failed_handler<F>(
    controller: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller,
    on_failed: F,
) -> std::result::Result<i64, String>
where
    F: Fn() + 'static,
{
    use webview2_com::ProcessFailedEventHandler;

    // SAFETY: `controller` is a valid live `ICoreWebView2Controller` borrowed from Tauri's
    // `with_webview` callback on the UI thread; `CoreWebView2()` returns an owned COM pointer.
    let webview = unsafe { controller.CoreWebView2() }.map_err(|err| err.to_string())?;
    let handler = ProcessFailedEventHandler::create(Box::new(move |_, _| {
        dispatch_process_failed(&on_failed);
        Ok(())
    }));
    let mut token: i64 = 0;
    // SAFETY: `webview` is a valid `ICoreWebView2`, `handler` is a COM event handler built by
    // `ProcessFailedEventHandler::create`, and `token` is a valid writable `i64` out-pointer.
    unsafe { webview.add_ProcessFailed(&handler, &mut token) }.map_err(|err| err.to_string())?;
    Ok(token)
}

/// Invokes a process-failed reset callback while isolating any non-fatal panic inside the handler
/// boundary so a COM callback never unwinds across FFI.
pub fn dispatch_process_failed<F: Fn()>(on_failed: &F) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(on_failed));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn dispatch_process_failed_runs_callback_and_contains_panics() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);
        dispatch_process_failed(&move || {
            calls_ref.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        dispatch_process_failed(&|| {
            std::panic::panic_any("simulated handler failure");
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
