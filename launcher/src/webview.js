// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

// Guards browser actions on every platform. The launcher supplies allowDevtools.

// Cancel defaults without intercepting the app's own keyboard handlers
document.addEventListener(
  "keydown",
  (event) => {
    const key = event.key.toLowerCase();
    const command = event.ctrlKey || event.metaKey;
    const browserShortcut =
      key === "f5" ||
      key === "f3" ||
      key.startsWith("browser") ||
      (command &&
        ["r", "f", "g", "p", "s", "u", "+", "=", "-", "0"].includes(key)) ||
      (event.altKey && ["arrowleft", "arrowright", "home"].includes(key));
    const inspectorShortcut =
      key === "f12" ||
      (event.ctrlKey && event.shiftKey && ["i", "j", "c"].includes(key)) ||
      (event.metaKey && event.altKey && ["i", "j", "c"].includes(key));

    if (browserShortcut || (!allowDevtools && inspectorShortcut)) {
      event.preventDefault();
    }
  },
  true,
);

// App controls and editing shortcuts provide actions without the browser menu
document.addEventListener(
  "contextmenu",
  (event) => event.preventDefault(),
  true,
);

// Trackpad pinch can arrive as a Ctrl+wheel event; ordinary scrolling stays enabled
document.addEventListener(
  "wheel",
  (event) => {
    if (event.ctrlKey || event.metaKey) event.preventDefault();
  },
  { capture: true, passive: false },
);

// WebKit exposes pinch zoom through gesture events
document.addEventListener("gesturestart", (event) => event.preventDefault(), {
  passive: false,
});
document.addEventListener("gesturechange", (event) => event.preventDefault(), {
  passive: false,
});
