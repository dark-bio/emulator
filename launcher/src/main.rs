// launcher: starts QEMU with the firmware image, then hands control to Tauri
// which manages a fixed-size native window hosting the emulator's UI. The
// QEMU lifecycle is tied to the process: closing the window exits the
// launcher and PR_SET_PDEATHSIG kills QEMU (Linux); QEMU exiting on its own
// terminates the launcher and the window closes with it.
//
//   ws clients ──TCP──▶ host:8080 ──QEMU SLIRP hostfwd──▶ guest:8080 ──▶ firmware

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;

struct Config {
    kernel: PathBuf,
    initrd: PathBuf,
    disk: PathBuf,
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
            // Backing block device for the firmware's eMMC partitions. Mutated
            // in-place by the guest (LUKS init, user data); rebuilding the
            // firmware artifact resets it.
            disk: firmware.join("disk.img"),
            host_addr: std::env::var("EMULATOR_HOST_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:8080".to_string()),
        }
    }
}

fn main() {
    let cfg = Config::from_env();

    for required in [&cfg.kernel, &cfg.initrd, &cfg.disk] {
        if !required.exists() {
            eprintln!(
                "missing {} -- set EMULATOR_FIRMWARE to a directory containing \
                 vmlinuz + initramfs.gz + disk.img (produced by the firmware \
                 repo's `./arkos.sh emulator-build`)",
                required.display()
            );
            std::process::exit(1);
        }
    }

    println!(
        "[launcher] firmware from {}, hardware bus at ws://{}/hw once guest boots",
        cfg.kernel.parent().unwrap().display(),
        cfg.host_addr,
    );

    // Spawn QEMU; a background thread watches it and exits the process if it
    // dies on its own, which causes Tauri's window to close with us.
    let qemu = spawn_qemu(&cfg);
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

    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn spawn_qemu(cfg: &Config) -> Child {
    let mut cmd = Command::new("qemu-system-aarch64");
    cmd.args([
        "-M",
        "virt",
        "-cpu",
        "cortex-a72",
        "-m",
        "8192",
        "-nographic",
        "-kernel",
    ])
    .arg(&cfg.kernel)
    .args(["-initrd"])
    .arg(&cfg.initrd)
    // rdinit=/sbin/init hands control to openrc inside the rootfs, which
    // brings up networking and starts runcore (the arkos-core supervisor).
    .args(["-append", "console=ttyAMA0 rdinit=/sbin/init", "-netdev"])
    .arg(format!("user,id=net0,hostfwd=tcp:{}-:8080", cfg.host_addr))
    .args(["-device", "virtio-net-pci,netdev=net0", "-drive"])
    // Backing disk for the firmware's eMMC partitions (boot/trya/tryb/self/user).
    // format=raw matches the layout produced by arkos-make-emmc.sh.
    .arg(format!(
        "file={},if=none,id=disk0,format=raw",
        cfg.disk.display()
    ))
    .args([
        "-device",
        "virtio-blk-pci,drive=disk0",
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
