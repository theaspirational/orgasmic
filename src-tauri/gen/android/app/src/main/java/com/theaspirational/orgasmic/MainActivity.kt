package com.theaspirational.orgasmic

import android.os.Bundle
import android.webkit.JavascriptInterface
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  // Latest system-bar + display-cutout insets in device px (top, right, bottom,
  // left). The WebView runs edge-to-edge (enableEdgeToEdge below), so it draws
  // under the status/navigation bars and the web layer is responsible for
  // insetting its own chrome. Android WebView doesn't reliably expose these via
  // CSS env(safe-area-inset-*) (system bars are omitted; Chromium < 140 reports
  // 0px), so we bridge the measured values to ui/src/lib/androidInsets.ts.
  // Written on the UI thread, read from the WebView's JS thread — @Volatile so
  // the bridge sees the latest reference.
  @Volatile
  private var insets = intArrayOf(0, 0, 0, 0)

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)

    // Capture insets on every dispatch (initial layout, rotation, bar changes).
    // Return them unconsumed so the WebView keeps drawing edge-to-edge.
    ViewCompat.setOnApplyWindowInsetsListener(webView) { _, windowInsets ->
      val bars = windowInsets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout(),
      )
      insets = intArrayOf(bars.top, bars.right, bars.bottom, bars.left)
      windowInsets
    }

    // The web layer pulls these on each document load — the listener above does
    // not re-fire when the same WebView navigates to a new origin (bootstrap ->
    // daemon UI) — and again on resize/rotation.
    webView.addJavascriptInterface(InsetsBridge(), "__orgasmicInsets")
  }

  private inner class InsetsBridge {
    // CSS px = device px / display density. Float.toString() is locale-neutral
    // (always '.'), so the JSON is safe for JSON.parse on any device locale.
    @JavascriptInterface
    fun get(): String {
      val d = resources.displayMetrics.density
      val top = insets[0] / d
      val right = insets[1] / d
      val bottom = insets[2] / d
      val left = insets[3] / d
      return "{\"top\":$top,\"right\":$right,\"bottom\":$bottom,\"left\":$left}"
    }
  }
}
