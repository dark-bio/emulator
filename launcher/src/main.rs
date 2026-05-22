// launcher: starts QEMU with the firmware image, opens a fixed-size native
// window hosting the emulator's UI (loaded into the OS webview), and ties the
// QEMU lifecycle to the window. Closing the window or QEMU's own exit both
// terminate the launcher process; PR_SET_PDEATHSIG ensures QEMU dies with us.
//
//   ws clients ──TCP──▶ host:8080 ──QEMU SLIRP hostfwd──▶ guest:8080 ──▶ firmware
//
// The webview (this launcher's own window) speaks the firmware's /hw channel;
// the demo dashboard in ../dashboard/ speaks /ws over its own browser tab.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;

use tao::dpi::LogicalSize;
use tao::event::{ElementState, Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::keyboard::KeyCode;
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

// Messages dispatched from background threads or the webview's IPC bridge into
// the Tao event loop, where they can safely touch the window.
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    /// JS pointerdown -- snapshot the current outer position so subsequent
    /// `DragBy` deltas can be added to it without depending on any assumed
    /// relationship between the cursor's window-relative coords and tao's
    /// outer-position coords (which can differ by a constant on GTK).
    DragStart,
    /// Cursor moved this many CSS pixels from the pointerdown position.
    DragBy { dx: f64, dy: f64 },
}

// All UI assets are baked into the binary at build time so the launcher
// stays a single self-contained executable. Served to the webview via a
// custom `wry://` protocol so the HTML can use ordinary relative paths
// (`src="assets/..."`) that also work when the file is opened directly in a
// browser for iteration.
const UI_HTML: &[u8] = include_bytes!("../../ui/index.html");
const UI_VAULT_SVG: &[u8] = include_bytes!("../../ui/assets/dark_bio_vault.svg");
const UI_TOOL_SVG: &[u8] = include_bytes!("../../ui/assets/sim_removal_tool.svg");

struct Config {
    kernel: PathBuf,
    initrd: PathBuf,
    host_addr: String,
}

impl Config {
    fn from_env() -> Self {
        let firmware = std::env::var("EMULATOR_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("firmware/build"));
        Self {
            kernel: firmware.join("vmlinuz"),
            initrd: firmware.join("initramfs.gz"),
            host_addr: std::env::var("EMULATOR_HOST_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8080".to_string()),
        }
    }
}

fn main() -> wry::Result<()> {
    // WebKitGTK's hardware-accelerated compositor breaks on certain
    // GPU/Wayland combinations, leaving the webview blank. Disabling the
    // DMA-BUF renderer falls back to a path that works everywhere. Only
    // touched on Linux; honors any explicit user setting.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: we are still single-threaded here; main has just started.
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }

    let cfg = Config::from_env();

    for required in [&cfg.kernel, &cfg.initrd] {
        if !required.exists() {
            eprintln!(
                "missing {} -- run firmware/build.sh first \
                 (or set EMULATOR_FIRMWARE to a directory containing vmlinuz + initramfs.gz)",
                required.display()
            );
            std::process::exit(1);
        }
    }

    println!(
        "[launcher] firmware from {}, WebSocket at ws://{}/ws once guest boots",
        cfg.kernel.parent().unwrap().display(),
        cfg.host_addr,
    );

    let qemu = spawn_qemu(&cfg);

    // If QEMU dies on its own (kernel panic, bad args, etc.), terminate the
    // process so the window comes down with it.
    thread::spawn(move || {
        let mut child = qemu;
        match child.wait() {
            Ok(status) => {
                eprintln!("[launcher] QEMU exited with {status}");
                std::process::exit(status.code().unwrap_or(0));
            }
            Err(e) => {
                eprintln!("[launcher] wait on QEMU failed: {e}");
                std::process::exit(1);
            }
        }
    });

    let event_loop: EventLoop<UserEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title("Ark I emulator")
        // 220×286 scene + 12px padding all sides = 244×310 inner, scaled ×1.5.
        .with_inner_size(LogicalSize::new(366.0, 465.0))
        .with_resizable(false)
        // Windowless look: no chrome, alpha shines through to the desktop.
        // Requires a compositor on X11; works on Wayland / macOS / Windows.
        .with_decorations(false)
        .with_transparent(true)
        .build(&event_loop)
        .expect("build window");

    let _webview = build_webview(&window, proxy)?;

    // Drag origin: the window's outer position at the moment the user pressed
    // the mouse, expressed in logical (CSS) pixels. Held across the event
    // loop so DragBy deltas can be added to it.
    let mut drag_origin: Option<tao::dpi::LogicalPosition<f64>> = None;

    event_loop.run(move |event, _, control_flow| {
        // Both window and the webview have to live as long as the event loop;
        // capturing them here keeps them alive for its lifetime.
        let _ = &_webview;
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::DragStart) => {
                drag_origin = window
                    .outer_position()
                    .ok()
                    .map(|phys| phys.to_logical::<f64>(window.scale_factor()));
            }
            Event::UserEvent(UserEvent::DragBy { dx, dy }) => {
                if let Some(start) = drag_origin {
                    window.set_outer_position(tao::dpi::LogicalPosition::new(
                        start.x + dx,
                        start.y + dy,
                    ));
                }
            }
            Event::WindowEvent { event, .. } => {
                let close = matches!(event, WindowEvent::CloseRequested)
                    || matches!(
                        &event,
                        WindowEvent::KeyboardInput { event, .. }
                            if event.state == ElementState::Pressed
                                && event.physical_key == KeyCode::Escape
                    );
                if close {
                    // Exit the whole process -- PDEATHSIG (Linux) then kills
                    // QEMU. On other OSes, QEMU outlives an externally-killed
                    // launcher (see protect_from_orphan below) so a graceful
                    // exit here still requires the kernel to clean up.
                    std::process::exit(0);
                }
            }
            _ => {}
        }
    });
}

// Serve the embedded UI bundle. The webview requests `wry://localhost/...`;
// we map "/" + "/index.html" to the HTML and "/assets/..." to the SVG bytes.
fn serve_asset(path: &str) -> (&'static [u8], &'static str) {
    match path {
        "/" | "/index.html" => (UI_HTML, "text/html; charset=utf-8"),
        "/assets/dark_bio_vault.svg" => (UI_VAULT_SVG, "image/svg+xml"),
        "/assets/sim_removal_tool.svg" => (UI_TOOL_SVG, "image/svg+xml"),
        _ => (b"not found", "text/plain"),
    }
}

// wry's cross-platform `WebViewBuilder::new(&window)` doesn't fully wire the
// webview into the GTK widget tree under Tao on Linux -- the window opens
// but stays blank. The Linux-specific `new_gtk(vbox)` path attaches it to
// the window's default vbox container, which is what actually renders.
fn build_webview(
    window: &tao::window::Window,
    proxy: EventLoopProxy<UserEvent>,
) -> wry::Result<wry::WebView> {
    let configure = move |builder: wry::WebViewBuilder<'_>| -> wry::Result<wry::WebView> {
        builder
            .with_custom_protocol("wry".to_string(), |request| {
                let path = request.uri().path().to_owned();
                let (body, mime) = serve_asset(&path);
                wry::http::Response::builder()
                    .header("Content-Type", mime)
                    .body(std::borrow::Cow::Borrowed(body))
                    .unwrap()
            })
            .with_url("wry://localhost/")
            .with_devtools(true)
            // Match the windowless look: the webview itself must not paint
            // its own background, otherwise it covers up the window's alpha.
            .with_transparent(true)
            // JS posts short string commands here; we route them into the Tao
            // event loop so they can safely call Window methods.
            //
            // We implement window drag ourselves rather than going through
            // tao's `Window::drag_window()` (which calls GTK's begin_move_drag):
            // that API needs the originating button event's serial/timestamp,
            // which our IPC hop strips off, leading to the call alternating
            // between success and a stale-grab failure.
            .with_ipc_handler(move |request| {
                let body = request.body();
                if body == "drag-start" {
                    let _ = proxy.send_event(UserEvent::DragStart);
                } else if let Some(rest) = body.strip_prefix("drag-by:") {
                    if let Some((sx, sy)) = rest.split_once(',') {
                        if let (Ok(dx), Ok(dy)) = (sx.parse::<f64>(), sy.parse::<f64>()) {
                            let _ = proxy.send_event(UserEvent::DragBy { dx, dy });
                        }
                    }
                }
            })
            .build()
    };

    #[cfg(target_os = "linux")]
    {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().expect("tao window has no default vbox");
        configure(WebViewBuilder::new_gtk(vbox))
    }
    #[cfg(not(target_os = "linux"))]
    {
        configure(WebViewBuilder::new(window))
    }
}

fn spawn_qemu(cfg: &Config) -> Child {
    let mut cmd = Command::new("qemu-system-aarch64");
    cmd.args([
        "-M",
        "virt",
        "-cpu",
        "cortex-a72",
        "-m",
        "256",
        "-nographic",
        "-no-reboot",
        "-kernel",
    ])
    .arg(&cfg.kernel)
    .args(["-initrd"])
    .arg(&cfg.initrd)
    .args(["-append", "console=ttyAMA0 rdinit=/init", "-netdev"])
    .arg(format!("user,id=net0,hostfwd=tcp:{}-:8080", cfg.host_addr))
    .args([
        "-device",
        "virtio-net-pci,netdev=net0",
        "-serial",
        "stdio",
        "-monitor",
        "none",
    ])
    .stdin(Stdio::null());

    protect_from_orphan(&mut cmd);

    cmd.spawn().unwrap_or_else(|e| {
        eprintln!("failed to spawn qemu-system-aarch64: {e}");
        std::process::exit(1);
    })
}

// On Linux, set PR_SET_PDEATHSIG so the kernel SIGKILLs QEMU if the launcher
// dies for any reason -- including SIGKILL or panic, neither of which run
// Rust's Drop. Without this the child gets reparented to PID 1 and keeps
// holding the host port.
//
// macOS has no equivalent prctl; Windows would use a Job Object with
// JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE. Both are deferred -- on those hosts
// QEMU may outlive an abnormally-terminated launcher.
#[cfg(target_os = "linux")]
fn protect_from_orphan(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec runs in the forked child before exec; only the
    // async-signal-safe prctl is called.
    unsafe {
        cmd.pre_exec(|| {
            let rc = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
            if rc == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn protect_from_orphan(_cmd: &mut Command) {}
