# Images

An emulated Ark is one image file, and the file is the device. It holds the
device's identity, its pairing and its data, so copying the file copies the
device and deleting it deletes the device. An image starts at a few
megabytes and grows as the guest writes, up to about 119 GiB, which is the
disk the guest sees.

The image is a plain qcow2 file, and the device's own keys sit inside it,
without the protection the hardware gives them. Anyone with the file holds
the device's identity. The owner's data stays encrypted while the device is
locked, and lies open in the memory of a process on this computer while it
is unlocked. Real data belongs on hardware.

Two emulators never boot one image. A foreground launch refuses an image
another emulator holds, and `ark-emulator start` waits for and reports that
emulator instead of booting a second one.

## Where things live

ark-emulator keeps its settings, its own image and its logs in one data
directory:

    macOS    ~/Library/Application Support/bio.dark.emulator
    Linux    ~/.local/share/bio.dark.emulator
    Windows  %APPDATA%\bio.dark.emulator

`ark-emulator doctor --json` prints the one this build uses, with the
settings file and the log directory inside it. The image the emulator
creates for itself, emulator.ark, lives there too, while an image named
with --image lives wherever its path says. Any file name works, and the
window's picker uses the .ark extension.

## Which image a start boots

settings.toml in the data directory remembers the image to boot, whether to
boot it when the window opens, the guest memory and the environment for a
new image. The window's settings panel writes it. The command line only
reads it, so `ark-emulator start --image` elsewhere leaves the remembered
image alone.

A start or a headless launch without --image boots the remembered image,
which is the device the owner opens by double-click, and creates it afresh
when the file is gone. With nothing remembered, it boots emulator.ark in
the data directory. A headless launch ignores the remembered autostart and
never opens a picker.

## Environments

An image is bound to one cloud environment, release, staging or develop,
when the firmware first boots on it. That first boot takes --env, or else
the environment set in the window, or else release. Later boots keep the
binding, so --env has no effect on an image that booted before, and a start
that names another environment warns once the device is up. Moving to
another environment takes a new image.

## Identity

A fresh emulated Ark has a self-signed identity. `ark enroll` gives it an
attested one, valid for 30 days, after which the cloud refuses the device.
`ark-emulator list` shows when it expires, and `ark genuine` reports when it
has. A copy of the image carries the identity and its expiry with it. A
factory reset with the button keeps the identity too, so an expired device
is wiped and started again as a fresh one.

## Wiping

`ark-emulator wipe` resets the image a start would boot to a fresh device,
and `ark-emulator wipe PATH` another image. The file is replaced by an empty
image at the same path, so the window and the next start find it as before,
and its device needs `ark enroll` and `ark pair` again. Pairing leaves the
device unlocked until its next boot. A booted image is refused with
disk-busy, so stop its emulator first. A terminal asks for confirmation,
and --yes gives it elsewhere.
