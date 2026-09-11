// Settings panel: a card that fades in over the device face.
//
// Its own element, with nothing of the info tray's geometry. The tray and the
// reset pin are device controls, so both step aside while it is up, and the
// gear that opens it goes with them.
//
// Two modes, decided by whether the guest has been started:
//
//   - startup: the launcher could not work out which image to boot. The panel
//     is up from the moment the window appears, says why, and answers three
//     ways: exit, start, or save and start. There is no device behind it yet,
//     so exit is the only other way out.
//   - running: opened from the gear in the info tray. The same controls,
//     written to the settings file for the *next* launch. The guest is never
//     disturbed.
//
// Nothing is written unless the button pressed says so, and then everything the
// form is showing is written. That is what puts the panel above the command
// line in the order of precedence: a flag seeds the form, and a form somebody
// looked at and saved settles it. Plain `start` is the way to take a flag for
// one run without it sticking. The launcher owns the truth in between, and
// `settings_state` is asked for it every time the panel opens, so an abandoned
// edit never survives.

/**
 * Wire up `panel` and the `gear` that opens it. `onGuest` runs once there is a
 * guest behind the device face, which is either at load, when the launcher
 * booted straight through, or the moment the startup form starts one.
 */
export function mountSettings({ panel, gear, onGuest }) {
  const note = panel.querySelector('.panel-note');
  const problem = panel.querySelector('.panel-error');
  const diskButton = panel.querySelector('.opt-disk');
  const remember = panel.querySelector('.opt-remember');
  const memory = panel.querySelector('.opt-memory');
  const envs = panel.querySelector('.opt-envs');
  const cancel = panel.querySelector('.opt-cancel');
  const start = panel.querySelector('.opt-start');
  const confirm = panel.querySelector('.opt-confirm');

  const invoke = (cmd, args) => window.__TAURI__?.core.invoke(cmd, args);

  // The launcher's last answer with whatever has been typed since folded into
  // it, which is also exactly the shape start and save take back.
  let state = null;
  let startup = false;

  function render() {
    note.textContent = state.note;
    cancel.textContent = startup ? 'exit' : 'cancel';
    start.hidden = !startup;
    confirm.textContent = startup ? 'save and start' : 'save';
    diskButton.textContent = state.name;
    diskButton.title = state.path;
    remember.setAttribute('aria-pressed', String(state.remember));
    memory.min = String(state.minMemory);
    memory.value = String(state.memory);
    renderEnvs();
  }

  function renderEnvs() {
    for (const button of envs.children) {
      button.setAttribute('aria-pressed', String(button.dataset.env === state.env));
    }
  }

  function buildEnvs() {
    envs.replaceChildren(...state.envs.map(env => {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'opt-env';
      button.dataset.env = env;
      button.textContent = env;
      button.addEventListener('click', () => {
        state.env = env;
        renderEnvs();
      });
      return button;
    }));
  }

  async function save() {
    busy(true);
    try {
      await invoke('save_settings', {
        disk: state.remember ? state.path : null,
        memory: state.memory,
        env: state.env,
      });
    } catch (e) {
      problem.textContent = String(e);
      busy(false);
      return;
    }
    leave();
  }

  // Fade out, dropping whatever was typed. The next open asks the launcher
  // again, so there is nothing local left to reset.
  function leave() {
    busy(false);
    panel.classList.remove('open');
  }

  function busy(working) {
    cancel.disabled = working;
    start.disabled = working;
    confirm.disabled = working;
    if (working) problem.textContent = '';
  }

  async function refresh() {
    const next = await invoke('settings_state');
    if (!next) return false;
    state = next;
    startup = state.mode === 'startup';
    // The tray and the pin hang off this: neither has anything to say until a
    // guest is behind the face.
    panel.classList.toggle('startup', startup);
    problem.textContent = '';
    buildEnvs();
    render();
    return true;
  }

  gear.addEventListener('click', async () => {
    await refresh();
    panel.classList.add('open');
  });

  // Clicking away from the panel is a cancel, the way it is for any other thing
  // that opens in front of something else. The capture phase is what makes it
  // only that: the tray is still sitting behind the panel, and without this the
  // same click would toggle it on the way past.
  //
  // The startup form is exempt. There is no device behind it to click back to,
  // and dropping it would leave a window with nothing in it.
  document.addEventListener('click', event => {
    if (startup || !panel.classList.contains('open')) return;
    if (panel.contains(event.target)) return;
    event.stopPropagation();
    event.preventDefault();
    leave();
  }, true);

  diskButton.addEventListener('click', async () => {
    const picked = await invoke('pick_disk', { current: state.path });
    if (!picked) return;
    state.path = picked.path;
    state.name = picked.name;
    problem.textContent = '';
    render();
  });

  remember.addEventListener('click', () => {
    state.remember = !state.remember;
    render();
  });

  // On change rather than on input, so a half-typed number is never read as
  // the answer.
  memory.addEventListener('change', () => {
    const value = Number.parseInt(memory.value, 10);
    if (!Number.isFinite(value) || value < state.minMemory) {
      problem.textContent = `That is too little memory. The least is ${state.minMemory} MiB.`;
      memory.value = String(state.memory);
      return;
    }
    problem.textContent = '';
    state.memory = value;
  });

  cancel.addEventListener('click', () => {
    // Nothing has been started, so there is nothing to leave to. Closing the
    // window is the way out of a launch that was a mistake, and is what Escape
    // does too.
    if (startup) {
      window.__TAURI__?.window.getCurrentWindow().close();
      return;
    }
    leave();
  });

  start.addEventListener('click', () => begin(false));

  confirm.addEventListener('click', () => (startup ? begin(true) : save()));

  // Boot the guest on what the form is showing. `persist` is the difference
  // between the two buttons that do it: `save and start` writes the form down
  // for the launches after this one, `start` takes it for this one only.
  async function begin(persist) {
    busy(true);
    try {
      await invoke('start_emulator', {
        disk: state.path,
        remember: state.remember,
        save: persist,
        memory: state.memory,
        env: state.env,
      });
    } catch (e) {
      problem.textContent = String(e);
      busy(false);
      return;
    }
    // The guest is up, so the panel fades out onto a device face that finally
    // has something behind it, and comes back as the next launch's settings.
    startup = false;
    panel.classList.remove('open', 'startup');
    busy(false);
    await refresh();
    onGuest();
  }

  refresh().then(ready => {
    // Outside the launcher there are no commands to answer and nothing gates a
    // start, so the page is the device face alone and behaves as though a guest
    // were already behind it.
    if (ready && startup) {
      panel.classList.add('open');
      return;
    }
    onGuest();
  });
}
