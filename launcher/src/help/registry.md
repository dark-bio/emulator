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
      "disk": "ark-disk.img",
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

`ready` says the firmware has accepted a client. `env`, `name`, `serial` and
`expiry` are absent until the device has reported them, and they are what the
launcher heard rather than anything it verified. Discovery is not identity: a
handshake with the device is.

## Heartbeats and stopping

Every launcher republishes itself once a second, and an entry that has not
been refreshed for 15 s is dropped, so an emulator that was killed outright
leaves the listing on its own. A launcher that exits normally withdraws itself
at once.

A stop rides on the same heartbeat. The request is recorded against the entry,
the launcher's next heartbeat is answered with `{"stop": true}`, and it shuts
down the way closing its window does. Nothing signals a process or looks up a
pid. A launcher too old to read its mailbox never answers, which is why a stop
gives up after --timeout and says to close the window instead.
