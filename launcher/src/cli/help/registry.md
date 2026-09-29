# The registry of running emulators

Anything that wants to find an emulator on this computer reads one loopback
service. There is no daemon: whichever launcher binds 127.0.0.1:18180 serves
it, every other launcher publishes itself into it, and when the host goes the
next launcher takes the port over. A refused connection means no emulator is
running, and it is an empty list rather than an error.

Most callers want `ark-emulator list`, or `ark devices`, which reads the same
service. The contract below is for a tool that reads it directly.

## Routes

    GET    /v1/instances          the listing
    POST   /v1/instances          a launcher publishing itself
    DELETE /v1/instances/<port>   a launcher withdrawing itself
    POST   /v1/instances/<port>/stop   ask that emulator to shut down

Every answer carries permissive cross-origin headers, so a page in a browser
can read the listing. A stop request answers 204 when the registry knows that
port and 404 when it does not.

## The listing

```
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
      "serial": "0b6f...",
      "expiry": 1788000000
    }
  ]
}
```

`version` is 1 and changes only when something breaks. Adding a field or a
route does not, so read what you know and ignore the rest.

`port` identifies the emulator and is what a client dials.
`ws://127.0.0.1:PORT/v1/usb` is the Ark's own bus and `/v1/hw` the device
face. `disk` is the image's file name and `disk_id` an opaque digest of where
it lives, which lets two launchers agree on an image without anybody
publishing a path. No path is ever published, since any page in any browser
can read a loopback port.

The Rust launcher owns the hardware connection in both window modes.
New launchers also publish an optional `control` object with a loopback HTTP
`port` and an opaque launch `id`. Older registry hosts may omit this field;
restart all emulators with the current build to enable button commands.

`ready` becomes true on the first nameplate and false when the hardware
connection drops. `env`, `name`, `serial` and
`expiry` are absent until the device has reported them, and they are what the
launcher heard rather than anything it verified. Discovery is not identity; a
handshake with the device is. The tool prints these under the names `ark
devices` uses, `image`, `environment` and `expires`, and the wire keeps its
own, since the listing is versioned on its own.

## Heartbeats and stopping

Every launcher republishes itself once a second, and an entry that has not
been refreshed for 15 s is dropped, so an emulator that was killed outright
leaves the listing on its own. A launcher that exits normally withdraws itself
at once.

A stop rides on the same heartbeat. The request is recorded against the entry,
the launcher's next heartbeat is answered with `{"stop": true}`, and it shuts
down the way closing its window does. Nothing signals a process or looks up a
pid, so a stop that is never collected gives up after --timeout. Close the
device window or interrupt a foreground headless process when it cannot
collect the request.

## Button control

`ark-emulator button press [EMULATOR]` and `button release [EMULATOR]` send
inputs directly to the selected launcher, independently of registry
heartbeats. The CLI and window hold the button independently. Either hold
keeps it pressed, and both clear on hardware disconnection.

The control endpoint accepts these routes:

    GET  /v1/button                 current connection and button state
    POST /v1/button/press           establish a CLI hold
    POST /v1/button/press/SECONDS   hold and schedule automatic release
    POST /v1/button/release         release the CLI hold

Requests carry `X-Ark-Emulator` with the advertised launch id and no body.
The GET response carries connected, generation as a decimal string, pressed
and cli_pressed. POST requests also carry `X-Ark-Generation` from that read,
so inputs cannot carry over into another connection. A 200 response confirms
hardware delivery or an already applied hold, with pressed, cli_pressed,
changed and release_after_seconds. The last field is the accepted interval
or null. It does not confirm completion of any resulting firmware operation.

The timed route accepts whole seconds from 0 s to 4294967295 s. With 0 s,
the worker releases immediately after delivering the press, then replies
with cli_pressed false and release_after_seconds 0. A positive timer starts
at delivery on the hardware worker. A new CLI press replaces the timer,
including cancellation when the new press has no duration. Release and
disconnection cancel it too. Expiry clears only the CLI hold, preserving an
active window hold. Older launchers reject the timed route with 404 without
pressing the button; the CLI never falls back to an untimed press.

A POST answers 409 when hardware cannot accept the input. A missing or wrong
launch id answers 412, and a missing or malformed generation answers 400.
An invalid release duration also answers 400 without changing the hold.
Requests with bodies answer 413. Browser origins answer 403, and the endpoint
permits no cross-origin requests. No command retries an uncertain input.
