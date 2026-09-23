// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

//! Disables browser interactions in every emulator window while preserving text editing.

/// Installs interaction guards before page scripts, including in the error window.
pub(crate) fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    let builder = tauri::plugin::Builder::new("app-interactions").js_init_script(format!(
        "(() => {{ const allowDevtools = {}; {} }})();",
        cfg!(debug_assertions),
        include_str!("webview.js")
    ));

    #[cfg(windows)]
    let builder = builder.on_webview_ready(|webview| {
        use crate::diagnostics::log;

        if let Err(error) = webview.with_webview(|native| {
            if let Err(error) = configure_windows(native) {
                log!("[launcher] could not disable browser interactions: {error}");
            }
        }) {
            log!("[launcher] could not access native webview: {error}");
        }
    });

    builder.build()
}

/// Disables WebView2 actions that can be handled outside the page's event listeners.
#[cfg(windows)]
fn configure_windows(webview: tauri::webview::PlatformWebview) -> windows_core::Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Settings3, ICoreWebView2Settings5, ICoreWebView2Settings6,
    };
    use windows_core::Interface;

    // SAFETY: Tauri runs with_webview on the UI thread. The controller and its
    // settings remain owned COM references throughout these calls.
    unsafe {
        let settings = webview.controller().CoreWebView2()?.Settings()?;
        settings
            .cast::<ICoreWebView2Settings3>()?
            .SetAreBrowserAcceleratorKeysEnabled(false)?;
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetAreDevToolsEnabled(cfg!(debug_assertions))?;
        settings.SetIsZoomControlEnabled(false)?;
        settings
            .cast::<ICoreWebView2Settings5>()?
            .SetIsPinchZoomEnabled(false)?;
        settings
            .cast::<ICoreWebView2Settings6>()?
            .SetIsSwipeNavigationEnabled(false)?;
    }
    Ok(())
}
