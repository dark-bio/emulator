// Local boot animation, played by the UI while the firmware is still bringing
// up its own LED driver. Stands in for the firmware's own boot LED sequence:
// four LEDs blink in turn, each going through CYCLES_PER_LED fade-in/fade-out cycles
// that get progressively faster, then staying on at BOOT_PWR while the next
// LED takes its turn. Once the first three LEDs are lit, the fourth keeps
// pulsing at the fastest cadence indefinitely -- a "still loading" indicator
// rather than a wrap-around restart -- until the caller invokes `stop()`.
//
// The animation doesn't write to the DOM itself -- each frame it computes the
// four [r,g,b] colour triples (each channel in 0..1) and hands them to the
// `render` callback the caller provided. In practice that callback is the
// same applyLeds() used for firmware-driven frames, so brightness boost and
// glow scaling stay consistent across the boot/runtime handoff.

// Boot LED colour, scaled by the per-frame brightness. Equivalent to
// hsl(353, 74%, 50%): scaling the whole RGB triple preserves channel ratios
// so the hue stays constant through the fade. Dropping HSL lightness instead
// would make the fade pass through red because HSL→RGB at low lightness
// collapses toward the dominant channel.
const BOOT_RGB = [0.87, 0.13, 0.216];
const BOOT_PWR = 0.2;  // 0..1, peak intensity multiplier
const NUM_LEDS = 4;
const APPROX_BOOT_TIME_S = 20;
const CYCLES_PER_LED = APPROX_BOOT_TIME_S / NUM_LEDS;
const STEPS_PER_FADE = 25;
const FADES_PER_CYCLE = 2;

// Per-step delay shrinks linearly as cycle index j grows, so blinks speed up
// as the LED's phase approaches completion.
const stepDelayMs = j => 10 + APPROX_BOOT_TIME_S / NUM_LEDS - 2 * (j + 1);

const LED_BLINK_MS = (() => {
  let total = 0;
  for (let j = 0; j < CYCLES_PER_LED; j++) {
    total += STEPS_PER_FADE * 2 * FADES_PER_CYCLE * stepDelayMs(j);
  }
  return total;
})();

function bootBrightness(ledIndex, elapsedMs) {
  const myStart = ledIndex * LED_BLINK_MS;
  if (elapsedMs < myStart) return 0;                  // hasn't started yet
  let inLed = elapsedMs - myStart;
  if (inLed >= LED_BLINK_MS) {
    // Blink phase done. Earlier LEDs latch on; the last LED keeps pulsing at
    // the fastest cadence so the boot animation reads as "still loading"
    // rather than "done" until the firmware actually takes over.
    if (ledIndex < NUM_LEDS - 1) return BOOT_PWR;
    const stepMs = stepDelayMs(CYCLES_PER_LED - 1);
    const subBlinkMs = STEPS_PER_FADE * 2 * stepMs;
    const fadeMs = STEPS_PER_FADE * stepMs;
    const inCycle = (inLed - LED_BLINK_MS) % subBlinkMs;
    const norm = inCycle < fadeMs
      ? inCycle / fadeMs
      : 1 - (inCycle - fadeMs) / fadeMs;
    return BOOT_PWR * norm;
  }
  for (let j = 0; j < CYCLES_PER_LED; j++) {
    const stepMs = stepDelayMs(j);
    const cycleMs = STEPS_PER_FADE * 2 * FADES_PER_CYCLE * stepMs;
    if (inLed < cycleMs) {
      const subBlinkMs = STEPS_PER_FADE * 2 * stepMs;
      const inCycle = inLed % subBlinkMs;
      const fadeMs = STEPS_PER_FADE * stepMs;
      const norm = inCycle < fadeMs
        ? inCycle / fadeMs                  // fade up 0..1
        : 1 - (inCycle - fadeMs) / fadeMs;  // fade down 1..0
      return BOOT_PWR * norm;
    }
    inLed -= cycleMs;
  }
  return BOOT_PWR;
}

/**
 * Start the boot animation. Each animation frame it computes the four LED
 * colours and passes them as a `[[r,g,b], [r,g,b], [r,g,b], [r,g,b]]` array
 * (channels in 0..1) to `render` -- typically the same applyLeds() used for
 * firmware-driven frames.
 *
 * Returns a controller with `stop()` to halt the RAF loop; the caller is
 * responsible for whatever gets painted after handoff.
 */
export function startBootAnimation(render) {
  let active = true;
  const start = performance.now();
  function tick() {
    if (!active) return;
    const elapsed = performance.now() - start;
    const colors = [];
    for (let i = 0; i < NUM_LEDS; i++) {
      const b = bootBrightness(i, elapsed);
      colors.push([BOOT_RGB[0] * b, BOOT_RGB[1] * b, BOOT_RGB[2] * b]);
    }
    render(colors);
    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);
  return { stop() { active = false; } };
}
