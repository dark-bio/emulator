# The registry of running emulators

The launcher holding 127.0.0.1:18180 serves browser discovery and a private
registry for native tools. Every launcher publishes into that registry. When
its host exits, the next launcher takes over both listeners. A missing
registry means an empty list. A timeout, refusal or malformed listing is a
discovery failure.

Most callers want `ark-emulator list`, or `ark devices`, which reads the same
service. The contract below is for a tool that reads it directly.

## Browser discovery

The loopback HTTP listener accepts `GET /v1/instances` and its OPTIONS
preflight. Every browser origin may read the listing. Host must name
127.0.0.1 or localhost with the discovery port. Registry writes and control
routes are absent from this listener, regardless of the request headers.

## Native transport

Native tools use HTTP/1.0 over local IPC, with one request per connection.
The registry endpoint is named `registry-18180`. Each launch's control
endpoint is named `c-ID`, where ID is its advertised launch id.

On Unix, these are filesystem sockets under `/tmp/ark-emulator-UID/`, where
UID is the effective user id. The directory has mode 0700 and sockets have
mode 0600. Persistent lock files protect live listeners during stale socket
recovery. On Windows, they are named pipes under
`\\.\pipe\ark-emulator-SID-NAME`, restricted to the current user's SID and
rejecting remote clients. Processes running as that user may connect.

The browser port has one owner across OS users. If it is held without a
private endpoint for the current user, close the other user's emulators,
then update and restart all emulators. Native tools never fall back to TCP.

The native registry serves these routes:

    GET    /v1/instances          the listing
    POST   /v1/instances          a launcher publishing itself
    DELETE /v1/instances/<port>   a launcher withdrawing itself

Publishing and withdrawing require `X-Ark-Registry: 1`. A missing, incorrect
or repeated header answers 403, as does a write carrying `Origin`. This is
a protocol marker; access control belongs to the socket or pipe. Headers
and bodies are each bounded to 8 KiB, and oversized requests answer 413.

Publishing and withdrawing answer 204 with no body. All running launchers
must support native IPC; restart them after updating.

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

`version` is 1. Read fields you know and ignore the rest.

`port` identifies the emulator and is what a client dials.
`ws://127.0.0.1:PORT/v1/usb` is the Ark's own bus and `/v1/hw` the device
face. `disk` is the image's file name and `disk_id` an opaque digest of where
it lives, which lets two launchers agree on an image without anybody
publishing a path. No path is ever published, since any page in any browser
can read a loopback port.

The Rust launcher owns the hardware connection in both window modes.
An optional `control` object contains an opaque launch `id` naming its native
endpoint. It carries no TCP port or filesystem path. A missing object makes
control unavailable and requires an update and restart.

`ready` becomes true on the first nameplate and false when the hardware
connection drops. `env`, `name`, `serial` and
`expiry` are absent until the device has reported them, and they are what the
launcher heard rather than anything it verified. Discovery is not identity; a
handshake with the device is. The tool prints these under the names `ark
devices` uses, `image`, `environment` and `expires`, and the wire keeps its
own, since the listing is versioned on its own.

## Heartbeats

Every launcher republishes itself once a second, and an entry that has not
been refreshed for 15 s is dropped, so an emulator that was killed outright
leaves the listing on its own. Shutdown attempts to withdraw the entry and
logs any failure without delaying shutdown for retries.

A lost registry connection triggers host takeover and republication. HTTP
refusals, timeouts and invalid replies fail the launch or stop the running
guest, with the cause reported in the window or terminal. A failed child
launch is reported by `start` with the launcher's log, without waiting for
the readiness timeout.

## Direct shutdown

`ark-emulator stop [EMULATOR]` selects its targets from one listing, then uses
their advertised control endpoints without reading the registry again. With
--all it stops the selected launches in port order, waiting for each to exit
before asking the next. A registry host exiting cannot lose the remaining
requests. A missing endpoint or an unsupported route fails with an update
and restart hint.

The control endpoint serves two lifecycle routes:

    GET  /v1/status   whether this launcher has accepted shutdown
    POST /v1/stop     accept shutdown and exit

Both carry `X-Ark-Emulator` with the advertised launch id and no body. Status
returns 200 with `{"stopping":false}` or `{"stopping":true}`. Stop returns
202 with `{"stopping":true}` before scheduling shutdown, even if the guest
is booting or disconnected. It needs no hardware connection generation.
Repeated requests acknowledge the same shutdown. The launch id prevents a
stale request stopping a replacement launcher.

The CLI sends the stop once and waits for the selected control endpoint to
disappear and the guest port to refuse connections. A lost or truncated
acknowledgement is checked through status without replaying the request.
An HTTP refusal or an invalid response fails with its explanation. The
remaining --timeout budget bounds every control request and guest probe;
confirmed stops remain in the partial result on failure or timeout.

Shutdown withdraws the entry best-effort and exits the launcher. Its orphan
protection terminates QEMU; no guest-level shutdown handshake takes place.
The entry can remain until its heartbeat expires if withdrawal failed.

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

A button POST answers 409 when hardware cannot accept the input or the
launcher is stopping. A missing, wrong or repeated launch id answers 412,
and a missing or malformed button generation answers 400.
An invalid release duration also answers 400 without changing the hold.
Requests with bodies answer 413. Browser origins answer 403, and the endpoint
permits no cross-origin requests. No command retries an uncertain input.
