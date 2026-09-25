// ark-emulator: emulated Ark enclave for development and demos
// Copyright 2026 Dark Bio AG. All rights reserved.
//
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

// Renders Rust device snapshots and forwards pointer interactions.

import { startBootAnimation } from "./boot_anim.js";

// Intensities used on hardware need a brightness boost on a screen
const BRIGHTNESS = 10 * 255;
// Glow dimensions in vmin at full display brightness
const GLOW_BLUR = 0.6;
const GLOW_SPREAD = 0.1;

/** Attach the device face to the launcher's state and input commands. */
export async function mountDevice({ pin, leds, onNameplate }) {
  const { invoke } = window.__TAURI__.core;
  let revision = -1;
  let phase = "idle";
  let connected = false;
  let generation = 0;
  let boot = null;
  let pressed = false;
  let reportedPressed = false;
  let inputs = Promise.resolve();

  // Preserve saturation and increase glow for intensities above display white
  function applyLeds(colors) {
    for (let i = 0; i < leds.length; i++) {
      const scaled = colors[i].map((channel) => channel * BRIGHTNESS);
      const overflow = Math.max(1, Math.max(...scaled) / 255);
      const [r, g, b] = scaled.map((channel) => Math.min(255, channel));
      leds[i].style.background = `rgb(${r}, ${g}, ${b})`;
      leds[i].style.boxShadow =
        `0 0 ${GLOW_BLUR * overflow}vmin ${GLOW_SPREAD * overflow}vmin ` +
        `rgba(${r}, ${g}, ${b}, 0.55)`;
    }
  }

  // Versioned snapshots keep a late initial reply from replacing newer state
  function render(state) {
    if (state.revision <= revision) return;
    revision = state.revision;
    connected = state.connected;
    pin.disabled = !connected;
    if (!connected || generation !== state.generation) {
      pressed = false;
      pin.classList.remove("pressed");
    }
    generation = state.generation;
    reportedPressed = state.pressed;
    pin.classList.toggle("pressed", reportedPressed);
    if (state.phase !== phase) {
      boot?.stop();
      boot = state.phase === "booting" ? startBootAnimation(applyLeds) : null;
      phase = state.phase;
    }
    if (phase === "firmware") applyLeds(state.colors);
    if (phase === "idle" || phase === "stopped") {
      applyLeds(Array.from({ length: 4 }, () => [0, 0, 0]));
    }
    onNameplate(state.nameplate);
  }

  // A snapshot request acknowledges the one pending change notification
  async function refresh() {
    try {
      render(await invoke("device_state"));
    } catch (error) {
      pin.disabled = true;
      pin.title = String(error);
    }
  }

  // Preserve input order even when the IPC replies arrive at different times
  function sendButton(value) {
    const connection = generation;
    inputs = inputs
      .then(() =>
        invoke("set_button_pressed", {
          pressed: value,
          generation: connection,
        }),
      )
      .catch((error) => {
        pin.title = String(error);
        pressed = false;
        refresh();
      });
  }

  // Pointer capture preserves the release when the pointer leaves the pin
  pin.addEventListener("pointerdown", (event) => {
    if (!connected || pressed) return;
    pin.setPointerCapture(event.pointerId);
    pressed = true;
    pin.classList.add("pressed");
    pin.title = "Device reset button";
    sendButton(true);
  });
  function release() {
    if (!pressed) return;
    pressed = false;
    pin.classList.toggle("pressed", reportedPressed);
    sendButton(false);
  }
  pin.addEventListener("pointerup", release);
  pin.addEventListener("pointercancel", release);
  pin.addEventListener("lostpointercapture", release);
  window.addEventListener("blur", release);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) release();
    else refresh();
  });
  pin.disabled = true;
  await window.__TAURI__.event.listen("device-state", refresh);
  await refresh();
}
