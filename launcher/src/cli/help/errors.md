# Errors

Each code is stable and belongs to one exit class. ark-emulator asks nobody
for approval, so running a command again only does its work again. Each
class below says when another run can help.

## Exit 1

Exit 1 is a problem with a local file, a confirmation or a launch. Another
run helps once its cause is fixed.

- `confirmation-required` means a wipe was not confirmed. Add --yes to
  `ark-emulator wipe`.
- `disk-missing` means the named image is not there. Check the path.
- `io` means a file could not be read or written, and the message names it.
  A headless run with no command that cannot start reports it too.
- `firmware-missing` means this build carries no firmware for the
  architecture. Pass --kernel and --initrd.
- `qemu-missing` means `ark-emulator doctor` found that the QEMU this build
  runs cannot run. Reinstall Ark Emulator, or install QEMU for a source
  build.
- `no-acceleration` means `ark-emulator doctor` found only software
  emulation, so a boot takes minutes. The hint names the fix for the
  platform.
- `port-exhausted` means every port from 18181 to 18280 is taken. Pass
  --port, or stop an emulator.
- `stopped-unexpectedly` means the launcher or the guest failed. A failed
  start quotes the end of the launcher's log.
- `could-not-start` means a run with no command could not bring the emulator
  up in its window. The message is what the error window shows.

## Exit 2

Exit 2 is `usage`, for invalid arguments or an unknown help topic. Another
run helps once the arguments are fixed, as the hint or the command's help
shows.

## Exit 3

Exit 3 is an emulator in the way or out of reach. Another run can succeed
once the emulator runs, is named and is free, unless the registry or the
control needs a newer Ark Emulator.

- `disk-busy` means an emulator holds that image. Stop it first.
- `no-emulator` means nothing running matches, or nothing runs.
  `ark-emulator list` shows what does.
- `ambiguous-emulator` means several emulators match, or several run and
  none was named. Name one by its locator, or pass --all to stop.
- `registry-unreachable` means the registry could not be read or refused a
  command. Emulators from different releases can cause it, so update Ark
  Emulator and restart every running emulator.
- `control-unsupported` means the emulator lacks the control the command
  needs. Update Ark Emulator and restart every running emulator.
- `control-unreachable` means the emulator's control could not be reached
  or understood. A failed stop may still be shutting down, so check
  `ark-emulator list` before stopping again. A button input without a reply
  has an unknown outcome, which `ark-emulator button release` clears.
- `button-unavailable` means the emulated hardware is disconnected or
  restarting, or could not take the input. Wait for the boot and try again,
  since an input never carries into a restarted guest.

## Exit 7

Exit 7 is `timeout`, for a start or a stop not confirmed within --timeout,
or a button reply that did not come. A start keeps booting and a stop keeps
shutting down, so check `ark-emulator list` before another run. A button
input's outcome is unknown, and `ark-emulator button release` clears it.

## Exit 130 and 143

Exit 130 is Ctrl-C and exit 143 SIGTERM. On Windows, Ctrl-Break counts as
Ctrl-C and a closed console as SIGTERM. During a start they end only the
wait, and the emulator keeps booting. A run with no command stops its
emulator.

- `interrupted` means Ctrl-C answered the wipe question, so the image stays
  as it was. Run `ark-emulator wipe` again to be asked again.
