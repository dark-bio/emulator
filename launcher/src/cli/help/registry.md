# The registry of running emulators

Every running emulator publishes itself into a registry on this computer,
which `ark-emulator list`, `ark devices` and Ark Hub read. Most callers want
one of those, and this topic is for a tool that reads the listing itself.

The registry answers `GET http://127.0.0.1:18180/v1/instances` with the
listing below, and the CORS preflight for it, to any browser origin. The
Host header names 127.0.0.1 or localhost with that port. Publishing,
withdrawing and controlling emulators go through private endpoints that only
the emulator's own processes use, so this listener serves only the listing.

A refused connection means no emulator is running, which is an empty list.
A timeout, a malformed listing or an HTTP failure means discovery is
incomplete, and a reader says so rather than showing an empty list.

## The listing

    {
      "version": 1,
      "instances": [
        {
          "port": 18181,
          "disk": "emulator.ark",
          "disk_id": "0b6f2f9c1d4e5a67",
          "ready": true,
          "env": "develop",
          "name": "test ark",
          "serial": "emulator-1234567-1",
          "expiry": 1788000000
        }
      ]
    }

- version is 1. A reader refuses a version it does not know, ignores fields
  it does not know, and reports an invalid entry without hiding the valid
  ones.
- port identifies the emulator. A client connects to the emulated Ark at
  ws://127.0.0.1:PORT/v1/usb, and `ark` names it emulator:PORT.
- disk is the image's file name, and disk_id an opaque digest of where it
  lives, which distinguishes two images of one name. No path is published,
  since any page in a browser can read the listing.
- ready turns true once the firmware has booted and reported itself, and
  false when the emulator's connection to its emulated hardware drops.
- env, name, serial and expiry appear once the firmware has reported them,
  as the emulator heard them and without any check. expiry is in Unix
  seconds. `ark` and ark-emulator show them as environment, name, serial and
  expires.
- control names a private endpoint for ark-emulator's own commands, and
  other readers ignore it.

Every emulator republishes itself once a second, and an entry not refreshed
for 15s drops out, so an emulator that was killed outright leaves the
listing on its own. The listing is discovery, not identity, since only a
handshake with the emulated Ark proves which device answered.
