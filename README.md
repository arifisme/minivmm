# minivmm

minivmm is a small virtual machine monitor for x86_64 Linux hosts, in the spirit of
Firecracker. KVM does the CPU virtualization; everything else is implemented here:
guest memory, the Linux boot protocol, page tables and GDT, ACPI tables, a 16550A UART,
an i8042 stub, a PCI bus, virtio 1.x over modern PCI with MSI-X, virtio-blk, virtio-net
on a TAP device, an epoll event loop, a JSON control socket, and snapshot/restore.

It boots an unmodified `bzImage` with multiple vCPUs, mounts an ext4 root from the
virtio disk, brings up networking, and can snapshot a running guest and resume it in a
fresh process.

## Host requirements

- x86_64 Linux with KVM and read/write access to `/dev/kvm`. The VMM checks for the
  capabilities it needs (in-kernel irqchip, PIT, irqfd, ioeventfd, GSI routing, XSAVE,
  vCPU events, immediate exit, ...) and refuses to start without them.
- Rust stable with edition 2024 support (1.85 or newer).
- For networking: a TAP interface the user may open (`scripts/setup-tap.sh`, run as
  root, creates one owned by a chosen user).
- For building guest artifacts: `git`, `gcc`, `make`, `flex`, `bison`, `bc`,
  `libelf-dev`, `libssl-dev`, `cpio`, `e2fsprogs` and `apt-get` (for Ubuntu's
  `busybox-static` package).

## Building

```
cargo build --release
scripts/build-guest.sh          # guest/out/{bzImage,initramfs.cpio.gz,rootfs.ext4}
sudo scripts/setup-tap.sh tap0 "$USER"
```

`build-guest.sh` clones Linux v6.12 from the GitHub mirror, starts from `tinyconfig`
and merges `guest/kernel.config` (only serial console, ACPI, PCI/MSI, virtio
blk/net, ext4, IPv4, SMP and the KVM paravirt clock), then builds a busybox initramfs
and a 64 MiB ext4 root filesystem containing busybox, a static dropbear and an init
script (`guest/rcS`) that configures `eth0` as `172.16.0.2/24` and starts dropbear.
Build products live in `guest/build` and `guest/out` and are ignored by git.

## Running

Boot the initramfs on the serial console:

```
target/release/minivmm --kernel guest/out/bzImage --initrd guest/out/initramfs.cpio.gz \
    --cpus 2 --mem 512M
```

Boot from the ext4 disk with networking and a control socket:

```
target/release/minivmm --kernel guest/out/bzImage \
    --cmdline "console=ttyS0 reboot=k panic=1 root=/dev/vda rw" \
    --cpus 2 --mem 512M --disk rootfs.ext4 --tap tap0 --api /tmp/minivmm.sock
ssh root@172.16.0.2              # empty password, served by dropbear in the guest
```

When stdin is a terminal it is switched to raw mode, so the guest console is fully
interactive (Ctrl-C goes to the guest); the terminal is restored on exit. The VMM exits
when the guest powers off or reboots, or on SIGINT/SIGTERM, after joining every vCPU
thread.

| Option | Meaning |
|---|---|
| `--kernel PATH` | bzImage to boot (boot protocol 2.12 or newer, 64-bit entry) |
| `--initrd PATH` | initramfs to load |
| `--cmdline TEXT` | kernel command line (default `console=ttyS0 reboot=k panic=1`) |
| `--cpus N` | number of vCPUs (default 1) |
| `--mem SIZE` | guest memory, e.g. `512M`, `4G` (default `512M`) |
| `--disk PATH` | raw disk image exposed as virtio-blk `/dev/vda` |
| `--disk-read-only` | expose the disk read-only (`VIRTIO_BLK_F_RO`) |
| `--tap NAME` | TAP interface backing a virtio-net device |
| `--mac ADDR` | guest MAC address (default `06:00:ac:10:00:02`) |
| `--api PATH` | Unix socket for the control API |
| `--restore PATH` | resume a snapshot instead of booting a kernel |
| `--no-seccomp` | do not install seccomp filters |
| `--log-level LEVEL` | `off`, `error`, `warn` (default), `info`, `debug`, `trace` |

### Control API

The API socket speaks one JSON object per line and answers with one JSON line:

```
$ echo '{"cmd":"status"}' | socat - UNIX-CONNECT:/tmp/minivmm.sock
{"memory_mib":512,"ok":true,"state":"running","vcpus":2}
```

| Request | Effect |
|---|---|
| `{"cmd":"status"}` | report `running`, `paused` or `stopping` |
| `{"cmd":"pause"}` | stop all vCPUs |
| `{"cmd":"resume"}` | restart all vCPUs |
| `{"cmd":"snapshot","path":"/tmp/vm.snap"}` | write a snapshot (pausing briefly if running) |
| `{"cmd":"shutdown"}` | stop the VM and exit |

Errors are reported as `{"ok":false,"error":"..."}`.

### Snapshot and restore

```
echo '{"cmd":"snapshot","path":"/tmp/vm.snap"}' | socat - UNIX-CONNECT:/tmp/minivmm.sock
echo '{"cmd":"shutdown"}' | socat - UNIX-CONNECT:/tmp/minivmm.sock
target/release/minivmm --restore /tmp/vm.snap --api /tmp/minivmm.sock
```

The snapshot records the machine configuration (vCPU count, memory size, disk path and
mode, TAP name and MAC), so a restore needs only the snapshot file; the disk image and
TAP device must still exist and the disk must not have been modified in between.

## Architecture

```
            main thread (epoll)                         vCPU threads (one per vCPU)
 stdin ─────► serial input        signalfd ─► exit        KVM_RUN loop
 TAP fd ────► virtio-net rx        exit eventfd           ├─ port I/O ─► PIO bus
 ioeventfd ─► virtio queue work    API socket + clients   └─ MMIO ────► MMIO bus
                    │                                              │
                    └──── irqfd (MSI-X GSI routes, serial GSI 4) ◄─┘
```

- **Threads.** The main thread owns the event loop: guest console input, the
  signalfd for SIGINT/SIGTERM, the exit eventfd, one ioeventfd per virtqueue, the TAP
  descriptor and the API sockets. Device emulation for virtqueues runs there. Each vCPU
  runs `KVM_RUN` on its own thread and dispatches port I/O and MMIO exits to the buses.
  Pause, resume, state capture and stop are requests sent over a channel; a real-time
  signal kicks the vCPU out of `KVM_RUN` by setting `immediate_exit` from the signal
  handler, so no request can be missed. Pausing first re-enters `KVM_RUN` with
  `immediate_exit` set so pending I/O completes before state is read.
- **Memory.** Guest RAM is anonymous `mmap` memory registered as KVM memory slots. Above
  3 GiB the RAM continues at 4 GiB, leaving `0xC0000000-0xFFFFFFFF` for PCI BARs, the
  IOAPIC, the LAPIC and KVM's TSS/identity map pages. Every guest access goes through
  `GuestMemory`, which bounds-checks the whole range and copies with raw pointers.
- **Boot.** The bzImage setup header is validated (boot flag, `HdrS`, protocol 2.12+,
  `XLF_KERNEL_64`), the protected-mode kernel is loaded at 1 MiB and entered at its
  64-bit entry point with a zero page (setup header copy, e820 map, command line,
  initrd placed below `initrd_addr_max`). The VMM writes a GDT with `__BOOT_CS`/
  `__BOOT_DS` at 0x10/0x18, identity-maps 4 GiB with 2 MiB pages and sets CR0, CR3, CR4
  and EFER for long mode.
- **CPU setup.** CPUID comes from `KVM_GET_SUPPORTED_CPUID` with per-vCPU APIC IDs,
  logical counts, cache sharing and extended topology (leaves 0x1, 0x4, 0xB/0x1F and the
  AMD leaves) rewritten for one socket with N cores. Boot MSRs, the FPU control words and
  LAPIC LINT0 (ExtINT) / LINT1 (NMI) are set on every vCPU.
- **Firmware tables.** RSDP, XSDT, FADT (with a PM1 event/control block), FACS, a DSDT
  with `\_S5` and the PCI root bridge (`PNP0A03` with `_CRS` windows), and a MADT that
  lists every vCPU and the IOAPIC. Writing `SLP_TYP=S5 | SLP_EN` to the PM1 control port
  powers the VM off; the i8042 reset command (`reboot=k`) reboots it.
- **Devices.** A `Bus` maps address ranges to devices for port I/O and MMIO. Port I/O
  hosts COM1 (16550A with FIFO, loopback and THRE/RDA interrupts through an irqfd on
  GSI 4), the i8042 reset path, the ACPI PM registers and PCI configuration mechanism #1
  at `0xCF8/0xCFC`.
- **PCI and virtio.** Bus 0 holds a host bridge and the virtio devices. Each virtio
  device has one 32 KiB memory BAR with the common, ISR, device and notify structures,
  the MSI-X table and PBA, and a capability list with all five virtio PCI capabilities
  (including the `VIRTIO_PCI_CAP_PCI_CFG` window) plus MSI-X. BAR sizing and relocation
  are supported; relocation moves the MMIO mapping and the ioeventfds. Feature
  negotiation requires `VIRTIO_F_VERSION_1`; the status register follows the virtio
  state machine and a malformed queue sets `DEVICE_NEEDS_RESET`. Split virtqueues
  validate ring placement, descriptor indices, chain length and every buffer range.
  MSI-X vectors map to KVM GSI routes and irqfds, with per-vector and function masking
  and pending bits.
- **virtio-blk** supports read, write, flush and get-id, a read-only mode and up to
  254 segments per request; I/O is done with positioned reads/writes through a bounded
  bounce buffer. **virtio-net** exchanges frames with a TAP opened with
  `IFF_VNET_HDR`, keeping one pending frame and pausing TAP polling while the guest
  has no receive buffers.
- **Snapshots.** The file is `MVMSNAP\0`, a format version, the length of a postcard
  encoded state blob, the blob, then each guest RAM region in address order. The state
  holds the machine configuration, PIC/IOAPIC/PIT/kvmclock state, the GSI routing
  table, per-vCPU CPUID, registers, special registers, XSAVE, XCRs, MSRs, LAPIC, MP
  state and pending events, the UART, i8042 and ACPI PM registers, the latched PCI
  configuration address and host bridge config space, and, for each virtio device, PCI
  configuration space, MSI-X table, transport registers and virtqueue indices.
- **Seccomp.** Before entering their loops, the main thread and each vCPU thread
  install their own BPF filter (built with seccompiler). `ioctl` is restricted to the
  request numbers each thread type needs and `mmap`/`mprotect` cannot create executable
  memory; any other syscall raises SIGSYS, whose handler reports the syscall number and
  exits.

Source layout: `src/memory.rs` (guest memory), `src/arch` (layout, GDT, paging,
registers, CPUID, ACPI/AML), `src/boot` (bzImage loader, zero page), `src/kvm` (VM,
vCPU, GSI routing), `src/devices` (bus, UART, i8042, ACPI PM, PCI, virtio),
`src/vmm` (machine construction and the event loop), `src/api.rs`, `src/json.rs`,
`src/snapshot.rs`, `src/seccomp.rs` and `src/sys` (signals, terminal, TAP).

## Testing

```
cargo test                                   # unit and integration tests
MINIVMM_GUEST_DIR=guest/out MINIVMM_TAP=tap0 cargo test --release -- --test-threads=1
cargo +nightly fuzz run virtqueue            # also: virtio_devices, device_registers,
                                             #       api_json, bzimage
```

Unit tests cover the boot structures and table builders, every device model, the
virtqueue code (including property tests) and the PCI/MSI-X logic. The integration
tests in `tests/` start the real binary and drive the guest over the serial console:
SMP boot with distinct APIC IDs, poweroff, reboot and SIGTERM handling, memory above
the MMIO gap, raw terminal handling, PCI enumeration with MSI-X, ext4 persistence across
a guest reboot, the read-only disk, ping/SSH/TCP in both directions over the TAP, the
control API, and snapshot/restore in a fresh process. They print a message and pass
without doing anything when `/dev/kvm`, the guest artifacts or the TAP device are
missing.

The GitHub Actions workflow enables KVM on the runner, builds (and caches) the guest
artifacts, creates `tap0`, runs fmt, clippy and the whole test suite, and prints and
uploads the guest console logs.

## Limitations

- x86_64 hosts and guests only; one socket, no SMT topology, no CPU hotplug.
- MSI-X is the only interrupt mechanism for PCI devices; there is no INTx fallback.
- virtio features are minimal: no indirect descriptors, event index, mergeable
  receive buffers, checksum or segmentation offloads, multiqueue or control queue.
- Block I/O is synchronous on the event loop thread; one disk and one NIC at most.
- Reboot ends the VMM process instead of resetting the machine.
- Snapshots copy all guest memory and are not incremental; restoring requires a host
  with a compatible CPU (the saved CPUID and MSRs are replayed as-is), the same disk
  image path and TAP name, and an unmodified disk image.
- The control API has no authentication beyond the socket's filesystem permissions.
