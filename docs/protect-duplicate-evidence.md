# Protect duplicate rows: evidence runbook

Owner-run, manual, bounded procedure for [#112](https://github.com/nredd/tethercam/issues/112)
(coordinated with [#33](https://github.com/nredd/tethercam/issues/33)). It says what to collect and
how to read it. It does not diagnose, fix or close anything, and it is not an acceptance test.

Limits:

- No root cause is established. Source reading shows one endpoint reference on every `Hello`,
  `ProbeMatches` and `ResolveMatches`, and a serial and ONVIF `HwAddress` derived only from
  `endpoint_uuid`. That rules out nothing about what Protect keys on
- Synthetic tests pin those invariants (`every_announcement_and_match_carries_one_stable_endpoint_and_a_fresh_message_id`,
  `the_onvif_identity_is_a_pure_function_of_the_endpoint_uuid`). They are not hardware or Protect
  evidence and prove nothing about row creation
- Never adopt a duplicate row, remove or re-adopt the working camera, or change identities or
  discovery to "see what happens". The adopted device stays as it is
- Re-adoption testing from the #112 acceptance list is a separate owner decision, not part of this

## Privacy

- Raw captures, controller screenshots and every real address, MAC, UUID, serial and name stay
  private: a local directory outside any repository (`0700`, files `0600`), deleted when the question is
  answered
- Publish only counts, yes/no equality results and timings relative to `t0`. Never paste a datagram,
  a UUID or an address, even partly
- Name identities by first-seen label (`E1`, `E2`, `H1`, `S1`) in notes. Labels, not values, go in
  the issue. Examples in this repository use `192.0.2.0/24` and obviously synthetic values only
- Anything that becomes a fixture (#33 golden probes) is first stripped of every identifier and
  reviewed as its own change

## Baseline

Record before any trigger, in one private note:

- `tethercam` commit, service state `ready` (the `camera` field of the authenticated
  `GET /tethercam/status`; read it privately, never put credentials in a URL), and that the camera
  is the adopted one online in Protect
- Protect row count for this endpoint: adopted rows and unadopted rows, as seen in the Devices view
- Clock offset between the capture host and the machine you read Protect on, to within 1 s (both
  NTP synced). Row timing is meaningless without it
- Which hosts could advertise the same camera: a second `tethercam` service, an old LaunchAgent, a
  copied `camera.toml` (same `endpoint_uuid`). Check and write down yes/no
- The capture privilege is arranged before the window opens. An interactive `sudo` prompt
  mid-window is a stop; do not weaken the host to get past one

## Capture

One window at a time, passive, on the `tethercam` host or a mirror port. Filter to WS-Discovery
only (UDP 3702, both groups, plus unicast). Hard caps, whichever comes first:

```sh
tcpdump -i <iface> -n -s 4200 -c 200 -G 600 -W 1 -w <private>/window1.pcap 'udp port 3702'
```

That is 200 packets and 10 minutes. The snap length fits an ordinary unfragmented WS-Discovery
frame: the largest accepted datagram (`MAX_PACKET`, 4096 bytes of payload) plus Ethernet, VLAN,
IPv6 and UDP headers, at most 4200 bytes each (under 1 MiB in total). It does not cover a
fragmented datagram. The filter matches at most the first fragment of a fragmented IPv4 datagram
(later fragments carry no UDP port) and none of a fragmented IPv6 one (the Fragment header hides
the port), so such a datagram is partly captured or missing. A fully captured first fragment is not
a complete envelope, and a missing reply may just be uncaptured. Leave the filter as it is.

Mark each stored packet that has the IPv4 more-fragments flag or a fragment offset set, or whose
captured length is below its original length, `fragmented` or `truncated` in the notes. Exclude it
from every equality comparison and count, never read a partial envelope as an identity, and treat
the window as insufficient evidence for any conclusion that depends on it. A missing reply is never
conclusive from this capture alone, since an uncaptured fragmented reply cannot be ruled out: it is
insufficient evidence, not a finding.

Start it first, then note `t0` (wall clock, UTC, seconds), then run the trigger:

- Window 1, observation only: do nothing for the window, only watch the Devices view
- Window 2, one service restart by the owner, however the service was installed (see
  [service](service.md)); never a re-adoption

Record each Protect row-count change as `t0 + N s`, with what the owner was doing (refresh,
navigating, nothing) at that moment.

## Read the capture offline

One line per datagram, private notes only: offset from `t0`, source label (`T` the camera host,
`C1`.. controllers, `X` anything else), dialect (2005/04 or 2009/01 namespace), action (`Hello`,
`Probe`, `ProbeMatches`, `Resolve`, `ResolveMatches`, `Bye`), `MessageID`, `RelatesTo`, endpoint
reference address, `XAddrs`, `Scopes`, `AppSequence` `InstanceId` and `MessageNumber`.

Expected, so not findings:

- Every `Hello`, `ProbeMatches` and `ResolveMatches` is sent after its own random delay of up to
  500 ms, so a reply can trail its probe by that long: allow 500 ms of slack when correlating row
  timing
- `Hello` once per served interface and dialect at startup, each after a random delay up to
  500 ms, so two per interface on a restart, plus the same pair when an interface appears while
  running
- On a graceful stop the service sends `Bye` per served interface and dialect (the same pair
  count as `Hello`), so a restart shows those before the new `Hello`s. A crash, kill or power loss
  sends none, and a missing `Bye` there is not a finding. No `Bye` is sent when an interface
  vanishes
- Replies are unicast to the prober and at most one per accepted probe. Retried or repeated copies
  of one `MessageID` are answered once. Whether Protect re-probes periodically is not established:
  record it, do not assume it
- `InstanceId` is the start time in seconds, so it changes across a restart and `MessageNumber`
  counts up within a run
- `XAddrs` may differ per interface: a reply carries the address of the interface inferred from the
  sender's subnet, not one observed (an open #33 concern), and IPv6 link-local addresses are never
  advertised
- `Scopes` are always the configured list, in every `Hello` and match: a difference is a finding

Multiple advertisements, each answered separately:

- Per probe: how many replies, from which `T` sources, in which dialects. More than one reply to
  one `MessageID` is a finding (#33 notes several sockets can exist on one interface index)
- Per interface and dialect: are the `Hello`s and matches identical in endpoint reference,
  `Scopes` and `XAddrs` apart from the allowed differences above
- Any datagram from an `X` source carrying the endpoint reference of this camera is a finding in
  itself: something else is advertising it

## Stable identity versus per-message IDs

The endpoint reference is the device identity: identical in every `Hello`, `ProbeMatches`,
`ResolveMatches` and the request to `Resolve`, across restarts (it is persisted as
`endpoint_uuid`). The WS-Addressing `MessageID` identifies one message and must be fresh every
time; `RelatesTo` must equal the request's `MessageID`. A fresh `MessageID` is correct protocol
behaviour, not a bug. Do not make it constant: this responder de-duplicates incoming `MessageID`s
(Protect's handling is unknown), so a constant value is at best unproven, and it would not touch
the identity.

## Private identity comparison

Compare by equality, not by value, and keep the values private. Fill one table per window in the
private note, each cell `same` or `differs` with the labels that differ:

- Endpoint reference: all advertisements, both dialects, both windows
- `XAddrs` host, port, path: per interface
- `Scopes`: all advertisements
- ONVIF `GetDeviceInformation` serial equals the endpoint reference's UUID, and `GetNetworkInterfaces`
  `HwAddress` equals the value derived from it (first byte `(b | 0x02) & 0xfe`, next five bytes
  unchanged). Check against the private config, never print the config
- What Protect shows per row (name, address, any identifier it displays): same or different between
  the adopted row and each extra row, using labels

## Interpreting results

Each is a hypothesis to test next, not a conclusion:

- Identity fields differ between advertisements: bridge-side. Report the shape (which field,
  between which labelled messages), then add a regression test with the fix
- Identical on the wire, and a row appeared with no `T` advertisement on UDP 3702 in the
  preceding seconds: inconclusive. The capture cannot see unicast or HTTP traffic between the
  controller and the camera elsewhere (another port, another host, a mirror gap), so it neither
  shows controller-side caching nor clears the bridge. Do not blame the cache or exonerate the
  bridge; seek controller-side evidence (what Protect reports for the row, or its logs if the owner
  chooses to look) before any conclusion
- Identical on the wire, and a row appeared right after a `ProbeMatches`: Protect may key on
  something other than the endpoint reference (source or `XAddrs` address, scopes, ONVIF
  responses). Needs controller-side evidence before any change
- More than one `T` source, interface or unexplained extra reply per probe: duplicate advertising;
  report counts and decide at #33 whether the interface or socket question is the cause
- An `X` source advertising the endpoint reference: stop and find the other advertiser first

## Stop criteria

Stop, record what was seen, and report it unresolved when any holds:

- A cap is hit (200 packets, 10 minutes) or any packet outside UDP 3702 got captured
- Two windows are done. More windows need a new hypothesis written down first
- A privilege prompt, credential prompt or any adoption or deletion prompt appears
- A new row appeared: record it and stop the window, do not interact with it
- The clock offset is unknown or over 1 s, or the baseline note is incomplete

## Report back

To #112 and #33, in the writing style of those issues and with nothing private: window count,
trigger, row-count timeline in seconds from `t0`, counts of advertisements per source label,
dialect and interface, the equality table, the count of `fragmented` or `truncated` packets and
whether each window is insufficient evidence (a missing reply always is), and which interpretation
above applies or that none does.
State that no hardware acceptance is claimed. Do not close either issue from this.
