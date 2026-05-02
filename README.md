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

`picokubelet` is a Kubernetes kubelet, written in Rust, targeting the ESP32-S3. It boots, gets a DHCP lease over Ethernet, talks TLS to a real k3s API server, registers itself as a node, and renews its lease so the control plane keeps believing it. As far as the cluster is concerned, it is a worker.

The hardware is a [Waveshare ESP32-S3-ETH]: an ESP32-S3R8 with a W5500 Ethernet controller on SPI, optionally PoE-powered. The control plane is a k3s instance on a Raspberry Pi. Nothing about that combination is unusual on its own; the unusual part is what's at the other end of the SPI bus.

It exists because nobody had told the ESP32 it couldn't.

[Waveshare ESP32-S3-ETH]: https://www.waveshare.com/esp32-s3-eth.htm

## What works and what's faked

Real:

- Bringing up the W5500 over SPI and getting an IPv4 address via DHCP.
- Reaching the k3s API server over TCP. (Phase 1; see status.)

Designed for:

- TLS to the API server using a static bearer token.
- `Node` object registration with capacity advertised honestly: `cpu: 240m`, `memory: 320Ki`, architecture `xtensa`, OS `embedded`.
- Lease renewal in `kube-node-lease` so the node stays `Ready`.
- Status updates with conditions reflecting actual board state — heap pressure becomes `MemoryPressure`.

Faked, by design:

- Pods. The kubelet accepts pod specs scheduled to it, acks them, and reports `Running`. It does not run them. There is no container runtime on a microcontroller. Pod execution is pure theatre.
- Volumes, networking, `exec`, logs. The pod-related endpoints return empty success or a plausible-looking error.
- The container runtime version. It reports `picortime://0.1.0`. There is no picortime.

## Hardware

- [Waveshare ESP32-S3-ETH](https://www.waveshare.com/esp32-s3-eth.htm) — ESP32-S3R8 plus a W5500, with an optional PoE module.
- A Raspberry Pi running k3s for the control plane. Any k3s install will do; the node doesn't care.

## Architecture

The ESP boots, brings up SPI, initialises the W5500, and lets embassy-net handle DHCP. Once it has an IP, it opens a TLS connection to the k3s API server. The address and bearer token are baked in at compile time via `env!`, sourced from a `.env` file loaded by `mise`.

The kubelet loop is two coroutines. One issues a `PATCH` against this node's `Lease` object every few seconds so the control plane keeps marking the node `Ready`. The other watches `/api/v1/pods?fieldSelector=spec.nodeName=picokubelet-XX` for pods scheduled here, and for each one walks the pod through its status transitions on a timer. There is no CRI, no runtime, no network namespace — just JSON patches saying yes, that pod is running, why do you ask.

The whole thing is built on `embassy` for async, `embassy-net` and `embassy-net-wiznet` for the network stack, and `esp-hal` for the chip.

## Building and flashing

You will need the Espressif Rust toolchain. The repo uses `mise` to pin everything (`espup`, `espflash`, the Xtensa-aware Rust toolchain) so you don't have to reason about it.

```sh
mise install
cp .env.example .env   # set K3S_API_HOST, K3S_API_PORT, bearer token
cargo build --release
espflash flash --monitor target/xtensa-esp32s3-none-elf/release/picokubelet
```

If you're new to Rust on Espressif chips, the [esp-rs book](https://docs.esp-rs.org/book/) is the right starting point. The Xtensa toolchain situation is what it is; `espup` makes it bearable.

## Status

Phase 1: the device boots, gets a DHCP lease, and completes a TCP three-way handshake against the k3s API server before disconnecting. The wires work.

Not yet written: TLS, the HTTP client, the kubelet loop, the pod-watching coroutine, any of the lease or status logic. The architecture section above describes intent, not what runs today. The README is ahead of the code on purpose — when it catches up, this section will say so.

## FAQ

**Should I use this in production?**
No.

**Why?**
It seemed like the obvious next step.

**Why Rust?**
The embedded Rust ecosystem on ESP32 — `esp-hal`, `embassy`, the `embedded-hal` traits — is currently the most pleasant way to write firmware. C would also work, in the same sense that you could also walk to the moon.

**Does it run Doom?**
No, but a pod scheduled to it can claim to.

## Acknowledgements

This wouldn't exist without the [esp-rs](https://github.com/esp-rs) working group, the [Embassy](https://embassy.dev/) project, and the maintainers of `embassy-net-wiznet`. All the actually-hard work — async on `no_std`, a TCP/IP stack that fits, a W5500 driver that doesn't lie about its DMA — is theirs. The novelty here is just pointing it at a Kubernetes cluster.

A [Specht Labs](https://specht-labs.de) project.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your option.
