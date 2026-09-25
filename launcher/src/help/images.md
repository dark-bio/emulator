# The image is the device

An emulated Ark is one `.ark` file. It holds everything the device knows: its
identity, its pairing, its data. Copy the file and you have copied the device;
delete it and the device is gone. Two emulators can never share one image,
since two guests writing one file would destroy it. A foreground launch refuses
a second boot of an image, and `start` waits for and reports the emulator that
already holds it instead of booting another.

The file is a plain qcow2 image with no encryption of any kind. It starts at a
few megabytes and grows as the guest writes, up to a virtual ceiling of about
119 GiB, which is what the guest sees as its disk. Anything the emulated Ark
stores is readable by anyone who can read that file, so real data belongs on
hardware.

## Where things live

The launcher keeps what it writes in one directory per platform:

    macOS    ~/Library/Application Support/bio.dark.emulator
    Linux    ~/.local/share/bio.dark.emulator
    Windows  %APPDATA%\bio.dark.emulator

`ark-emulator doctor --json` prints the one this build uses, along with the
settings file and the log directory inside it. The image the launcher
allocates for itself, `emulator.ark`, lives there too; an image you name with
--image lives wherever you put it. Any name works, and the window's own
picker uses the `.ark` extension so the files are easy to tell apart.

## What is remembered

`settings.toml` in that directory holds the image to boot, whether to boot it
without asking, the guest memory, and the environment a newly created image is
bound to. The window's settings panel writes it. The command line reads it and
never writes it, so a `start --image` somewhere else leaves the remembered
choice alone.

A start or headless launch with no --image takes the remembered image, which
is the device the owner opens by double-click. It creates that image afresh
if it is gone. Only when nothing was ever remembered does it take the image
the launcher allocates for itself.
Headless launches ignore the saved autostart toggle and never open a picker.

## Environments

An image is bound to one cloud environment, release, staging or develop, when
the firmware first boots on it. The binding is burnt in, so --env has no
effect on an image the firmware has booted before. Since it is read back from
the firmware, a start can only warn about a mismatch once the device is up.
To move to another environment, create a new image.

## Identity

An emulated Ark starts with a self-signed identity. `ark enroll` gives it an
attested one from the cloud, valid for 30 days, after which the cloud refuses
the device. `ark-emulator list` shows the day it expires, and `ark genuine`
says when it has passed. Copying the image copies the identity and its expiry
with it. A device past its date is wiped and started again as a fresh one; an
emulator holds nothing worth keeping that long.

## Wiping

`ark-emulator wipe` resets the image start would boot to a fresh device, and
`wipe PATH` another one. The file is replaced by an empty image at the same
path, so the window and the next start find it as before, and the device on
it is factory fresh, needing `ark enroll`, `ark pair` and `ark unlock` again.
A booted image is refused; stop the emulator first.
