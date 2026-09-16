// Settings panel: a card that fades in over the device face.
//
// Its own element, with nothing of the info tray's geometry. The tray and the
// reset pin are device controls, so both step aside while it is up, and the
// gear that opens it goes with them.
//
// Two modes, decided by whether the guest has been started:
//
//   - startup: autostart is disabled or the launcher needs an image to boot.
//     The panel is up from the moment the window appears and answers three
//     ways: exit, start, or save and start. There is no device behind it yet,
//     so exit is the only other way out.
//   - running: opened from the gear in the info tray. The same controls,
//     written to the settings file for the *next* launch. The guest is never
//     disturbed.
//
// New creates an image immediately. Preferences are written only on save,
// and then everything the form is showing is written. That puts the panel
// above the command line: a flag seeds the form, and a form somebody
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
  const diskName = panel.querySelector('.opt-disk');
  const openDisk = panel.querySelector('.opt-open');
  const newDisk = panel.querySelector('.opt-new');
  const autostart = panel.querySelector('.opt-autostart');
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
  let working = false;
  let loadFailed = false;

  function render() {
    note.textContent = state.note;
    cancel.textContent = startup ? 'exit' : 'cancel';
    start.hidden = !startup;
    confirm.textContent = startup ? 'save and start' : 'save';
    diskName.textContent = state.name;
    diskName.title = state.path;
    autostart.setAttribute('aria-pressed', String(state.autostart));
    memory.min = String(state.minMemory);
    memory.value = String(state.memory);
    envs.value = state.env;
    start.disabled = working || !state.path;
    confirm.disabled = working || !state.path;
  }

  function buildEnvs() {
    envs.replaceChildren(...[...state.envs].reverse().map(env => {
      const option = document.createElement('option');
      option.value = env;
      option.textContent = env;
      return option;
    }));
  }

  async function save() {
    busy(true);
    try {
      await invoke('save_settings', {
        disk: state.path,
        autostart: state.autostart,
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

  function busy(value) {
    working = value;
    for (const control of panel.querySelectorAll('button, input, select')) {
      control.disabled = value || loadFailed;
    }
    cancel.disabled = value;
    start.disabled = value || loadFailed || !state?.path;
    confirm.disabled = value || loadFailed || !state?.path;
    if (value) problem.textContent = '';
  }

  async function refresh() {
    let next;
    try {
      next = await invoke('settings_state');
    } catch (e) {
      loadFailed = true;
      state = null;
      note.textContent = 'Could not load the saved settings.';
      problem.textContent = String(e);
      diskName.textContent = '';
      diskName.title = '';
      memory.value = '';
      autostart.setAttribute('aria-pressed', 'false');
      envs.replaceChildren();
      start.hidden = true;
      cancel.textContent = 'cancel';
      confirm.textContent = 'save';
      busy(false);
      return false;
    }
    if (!next) return false;
    loadFailed = false;
    state = next;
    startup = state.mode === 'startup';
    // The tray and the pin hang off this: neither has anything to say until a
    // guest is behind the face.
    panel.classList.toggle('startup', startup);
    problem.textContent = '';
    buildEnvs();
    render();
    busy(false);
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
    if (!working) leave();
  }, true);

  async function chooseDisk(create) {
    busy(true);
    try {
      const picked = await invoke('pick_disk', { current: state.path, create });
      if (!picked) return;
      state.path = picked.path;
      state.name = picked.name;
      render();
    } catch (e) {
      problem.textContent = String(e);
    } finally {
      busy(false);
    }
  }

  openDisk.addEventListener('click', () => chooseDisk(false));
  newDisk.addEventListener('click', () => chooseDisk(true));

  autostart.addEventListener('click', () => {
    state.autostart = !state.autostart;
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

  envs.addEventListener('change', () => {
    state.env = envs.value;
  });

  cancel.addEventListener('click', () => {
    // Nothing has been started, so there is nothing to leave to. Closing the
    // window is the way out of a launch that was a mistake.
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
        autostart: state.autostart,
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
