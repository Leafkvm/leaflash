# leaflash

Development CLI for the [LeafKVM](https://www.crowdsupply.com/leafkvm/leafkvm) device.

```text
leaflash flash --image <IMAGE> --rootfs-size 256MiB
leaflash tui
leaflash usb <subcommand>     # nested rockusb-cli
```

## Subcommands

- **flash** — Erases the SD card, writes a fresh GPT with `rootfs_a` / `rootfs_b` /
  `userdata` (A and B sized identically), then writes the image into `rootfs_a`.
- **tui** — Interactive terminal UI: device info, file picker, size input
  (default rounded up to 128 MiB), Flash button.
- **usb** — Low-level rockusb operations (list, read/write LBA, erase, switch
  storage, reset, etc.) reused from the `rockusb-cli` library.

## Build

```sh
cargo build --release
```

Releases for Linux (musl), macOS, and Windows are produced by the `Release`
workflow on every `v*` tag.
