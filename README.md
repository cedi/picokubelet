# picokubelet

```bash
$ kubectl get nodes -owide
NAME                   STATUS   ROLES                  AGE     VERSION               INTERNAL-IP     EXTERNAL-IP   OS-IMAGE                         KERNEL-VERSION         CONTAINER-RUNTIME
clusterpi-leader       Ready    control-plane,master   546d    v1.31.1+k3s1          192.168.0.103   <none>        Debian GNU/Linux 12 (bookworm)   6.12.34+rpt-rpi-2712   containerd://1.7.21-k3s2
clusterpi-worker1      Ready    <none>                 546d    v1.31.1+k3s1          192.168.0.188   <none>        Debian GNU/Linux 12 (bookworm)   6.12.34+rpt-rpi-v8     containerd://1.7.21-k3s2
clusterpi-worker2      Ready    <none>                 546d    v1.31.1+k3s1          192.168.0.92    <none>        Debian GNU/Linux 12 (bookworm)   6.12.34+rpt-rpi-v8     containerd://1.7.21-k3s2
clusterpi-worker3      Ready    <none>                 546d    v1.31.1+k3s1          192.168.0.190   <none>        Debian GNU/Linux 12 (bookworm)   6.6.74+rpt-rpi-v8      containerd://1.7.21-k3s2
esp-node-01-guenther   Ready    <none>                 4m53s   v1.31.1-picokubelet   192.168.0.111   <none>        picokubelet on bare metal        esp-rs-no_std          lies://0.1.0
```

[![asciicast](https://asciinema.org/a/1004944.svg)](https://asciinema.org/a/1004944)

## What this is

`picokubelet` is a Kubernetes kubelet, written in Rust, targeting the ESP32-S3. It boots, gets a DHCP lease over Wi-Fi, talks TLS to a real k3s API server, registers itself as a node, and renews its lease so the control plane keeps believing it. As far as the cluster is concerned, it is a worker.

The hardware is a [Waveshare ESP32-S3-ETH]: an ESP32-S3R8 with a W5500 Ethernet controller on SPI, optionally PoE-powered. The control plane is a k3s instance on a Raspberry Pi. Nothing about that combination is unusual on its own; the unusual part is what's at the other end of the SPI bus.

It exists because nobody had told the ESP32 it couldn't.

[Waveshare ESP32-S3-ETH]: https://www.waveshare.com/esp32-s3-eth.htm

## What works and what's faked

Real:

- Wi-Fi association, DHCP, and TLS to a real k3s API server. (PoE+Ethernet via the on-board W5500 is the intended production form factor; the code path isn't there yet, so dev currently runs over Wi-Fi.)
- Wall-clock anchoring from the `Date` header on the first `/version` response. The board has no RTC, only vibes.
- `Node` registration via `POST /api/v1/nodes`, with capacity advertised honestly: `cpu: 240m`, `memory: 320Ki`, arch `xtensa-lx7`, OS `no_std`.
- Lease creation in `kube-node-lease`, plus renewal at a 10s cadence with a 40s lease duration.
- `/status` subresource PATCH on a 5min cadence, or immediately whenever any tracked condition flips.
- `MemoryPressure` driven by actual ESP heap free bytes; flips True below 20KB free.
- `lastTransitionTime` semantics: only updated when a condition's `status` field actually flips, not on every heartbeat. Conflating the two is a real-kubelet anti-pattern that makes nodes look flappy in monitoring.
- A `node.specht.dev/heap-bytes-free` annotation on the main resource that updates with each status push.
- Five custom joke-but-honest conditions (`Vibes`, `Caffeinated`, `Existential`, `Peckish`, `Haunted`) derived from real state (heap %, uptime, lifetime renewal count, Wi-Fi reconnects, BSSID changes, wall-vs-monotonic skew) and surfaced through `kubectl describe node`.
- WS2812 status LED on GPIO 21, driven via RMT from a dedicated task, with `SelfTest`, `Booting`, `Connecting`, `Healthy`, and `Activity` patterns.

Faked, by design:

- Pods. The kubelet does not yet watch `/api/v1/pods`. Promtail has scheduled itself onto the node and sits in `Pending`. That's the next phase of work.
- Volumes, `exec`, logs, `kubectl exec`, container runtime. None of these exist. The container runtime version is reported as `lies://0.1.0`.
- `PIDPressure` and `DiskPressure` are always reported as False. There are no PIDs, and there is no disk.

On the roadmap (in roughly this order):

- W5500 Ethernet swap. The board hardware is ready; the code path isn't.
- Pod theater: watching `/api/v1/pods?fieldSelector=spec.nodeName=…`, accepting scheduled work, and walking pods through their status transitions on a timer without ever running anything.
- OTA via OCI artifacts. The recursive joke being that the cluster deploys its own workers.
- The eight-node Tamagotchi rack. Boards on order.

## What it survived

At time of writing, `esp-node-01-guenther` has been `Ready` for <!-- TODO: actual count at publish time --> consecutive lease renewals, including survival of a real `BeaconTimeout` event with RSSI -90 dBm. The reconciler architecture recovered without intervention; the lease counter continued unbroken through the reconnection.

- Wi-Fi disconnect and re-association: handled
- TLS connection reset mid-request: handled
- Routes vanishing while reconcilers fire: handled

## Hardware

- [Waveshare ESP32-S3-ETH](https://www.waveshare.com/esp32-s3-eth.htm): ESP32-S3R8 plus a W5500, with an optional PoE module.
- A Raspberry Pi running k3s for the control plane. Any k3s install will do; the node doesn't care.

## Architecture

The ESP boots, brings up Wi-Fi via `esp-radio`, and lets `embassy-net` handle DHCP. Once it has an IP, `kubelet::bootstrap` walks through anchoring the wall clock from the `/version` `Date` header, registering the Node, creating the Lease, and pushing an initial status PATCH so every condition has a fresh `lastHeartbeatTime` out of the gate. The API server address, bearer token, and Wi-Fi credentials are baked in at compile time via `env!`, sourced from a `.env` file loaded by `mise`.

After bootstrap, two `embassy` tasks run the syncloop:

- `reconcilers::lease` PATCHes `kube-node-lease/<name>` every 10s (against a 40s lease duration), recreates the Lease on 404, and drives the LED.
- `reconcilers::status` PATCHes `/api/v1/nodes/<name>/status` every 5min (or immediately on any condition flip) and then PATCHes the heap-free annotation on the main resource.

Both tasks share a single `ApiClient` wrapped in an `embassy_sync::Mutex`, acquired briefly per request. The LED runs as its own independent task and listens on an `embassy_sync::Signal`, so it stays responsive even when the network code is mid-handshake. There is no CRI, no runtime, no network namespace; just JSON patches saying yes, this node is `Ready`, why do you ask.

Module map:

- `kubelet`: `NodeIdentity` and the bootstrap sequence (anchor clock → register node → create lease → push initial status).
- `k8s::{conditions, models}`: per-condition tracker state, plus typed Kubernetes resource builders that own their own JSON serialisation (no `serde`; the bodies are short and fixed-shape).
- `net::{wifi, http, client}`: Wi-Fi controller task, the parsed HTTP response shape, and the TLS+HTTP `ApiClient` built on `embedded-tls`.
- `reconcilers::{lease, status}`: the two syncloop tasks.
- `led`: the WS2812 driver task.

The whole thing is built on `embassy` for async, `embassy-net` for the network stack, `esp-radio` for Wi-Fi, `embedded-tls` for TLS, and `esp-hal` for the chip.

This is shaped like a real kubelet's syncloop, smaller.

## Status LED

The Waveshare board has a single onboard WS2812 on GPIO 21. Firmware drives it from a dedicated embassy task over RMT channel 0, so the LED keeps animating even when the kubelet is mid-TLS handshake. Brightness is capped at ~15%; full power is genuinely painful indoors.

| Pattern | State |
| --- | --- |
| Red → green → blue → off, 200 ms each | Self-test at boot; confirms the LED is alive before anything else runs. |
| Solid dim white | Booting. Set after self-test, before network init. |
| Blue, ~1.5 Hz breathe | Connecting. Covers Wi-Fi association, DHCP, TLS handshake, node registration, and initial Lease creation. |
| Green, ~0.33 Hz breathe | Healthy. Set after the *first* successful Lease renewal; Lease creation alone isn't enough. |
| Yellow flash, 100 ms | Lease renewal heartbeat, every ~10 s, overlaid on the green breathe. |

`Warning`, `Disconnected`, and `Panic` exist as enum variants but currently render to off. They're reserved for phases when error handling is built out enough to drive them honestly; an LED that lies under stress is worse than one that goes dark.

## Building and flashing

You will need the Espressif Rust toolchain. The repo uses `mise` to pin everything (`espup`, `espflash`, the Xtensa-aware Rust toolchain) so you don't have to reason about it.

```sh
mise install
cp .env.example .env   # set K3S_API_HOST, K3S_API_PORT, K3S_TOKEN, WIFI_SSID, WIFI_PSK, NODE_NAME
cargo build --release
espflash flash --monitor target/xtensa-esp32s3-none-elf/release/picokubelet
```

The dev loop currently runs over Wi-Fi; PoE+Ethernet via the on-board W5500 is the intended production setup but the code path isn't there yet. If you're new to Rust on Espressif chips, the [esp-rs book](https://docs.esp-rs.org/book/) is the right starting point. The Xtensa toolchain situation is what it is; `espup` makes it bearable.

## FAQ

**Should I use this in production?**
No.

**Why?**
It seemed like the obvious next step.

**Why Rust?**
The embedded Rust ecosystem on ESP32 (`esp-hal`, `embassy`, the `embedded-hal` traits) is currently the most pleasant way to write firmware. C would also work, in the same sense that you could also walk to the moon.

**Does it run Doom?**
No, but a pod scheduled to it can claim to.

## Acknowledgements

This wouldn't exist without the [esp-rs](https://github.com/esp-rs) working group, the [Embassy](https://embassy.dev/) project, and the maintainers of `embassy-net-wiznet`. All the actually-hard work (async on `no_std`, a TCP/IP stack that fits, a W5500 driver that doesn't lie about its DMA) is theirs. The novelty here is just pointing it at a Kubernetes cluster.

A [Specht Labs](https://specht-labs.de) project.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
