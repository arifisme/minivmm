# Progress

## Environment
- Local sandbox: no `/dev/kvm` and no vmx/svm CPU flag, so KVM cannot run here. Guest
  artifacts were built and sanity-checked locally under QEMU TCG (no KVM involved).
- End-to-end verification runs on GitHub Actions (`ubuntu-latest`, AMD EPYC host with
  SVM, Linux 6.17, KVM enabled through the udev rule from the task). Results are read
  through the Actions REST API; the workflow prints a test summary and every guest
  console log.
- Guest artifacts: Linux v6.12 from the GitHub mirror (kernel.org is blocked here),
  Ubuntu's `busybox-static` package, dropbear 2024.86 built statically from GitHub.

## Definition of done
| # | Item | Status | Verified by |
|---|------|--------|-------------|
| 1 | KVM core: VM/vCPUs, memory slots with the 3-4 GiB MMIO gap, irqchip, PIT, TSS and identity map, CPUID topology, MSRs, FPU, LINT0/1, GDT, page tables, CR0/CR3/CR4/EFER | done | unit tests (gdt, paging, regs, cpuid, layout); CI boots, `memory_above_the_mmio_gap_is_usable` (4 GiB guest) |
| 2 | Boot protocol: header validation, zero page, e820, cmdline, initrd placement | done | unit tests (bzimage, params); every CI boot |
| 3 | SMP and clean shutdown (poweroff, reboot, SIGINT/SIGTERM, threads joined, terminal restored) | done | CI: 4 vCPUs with distinct APIC IDs, poweroff, reboot, SIGTERM, pty raw-mode test |
| 4 | Bus, 16550A with IRQs and raw stdin, i8042 reset, irqfd, ioeventfd, epoll loop | done | unit tests for bus/serial/i8042/ACPI PM/event loop; CI console tests |
| 5 | PCI: host bridge, mechanism #1, BAR sizing/allocation/relocation, capabilities | done | unit tests (config, root, transport); CI `guest_enumerates_pci_devices` |
| 6 | virtio 1.x modern PCI, feature negotiation, status machine, split queues, MSI-X via GSI routing | done | unit/property tests (queue, transport, MSI-X); CI checks PCI-MSIX interrupts in the guest |
| 7 | virtio-blk: read, write, flush, get-id, read-only; ext4 root persists across reboot | done | unit tests; CI `ext4_root_persists_writes_across_reboot`, `read_only_disk_rejects_writes` |
| 8 | virtio-net on TAP: guest IP, ping, TCP both ways | done | CI `guest_network_reaches_host_both_ways` (ping, dropbear SSH banner, 200 KB in, 300 KB out) |
| 9 | Control API: status, pause, resume, snapshot, shutdown | done | unit tests (parser, server); CI `control_socket_pauses_resumes_and_shuts_down` |
| 10 | Snapshot/restore in a fresh process | done | CI `snapshot_restores_in_a_fresh_process` (console state, SMP, disk write, ping after restore) |
| 11 | Hardening: per-thread seccomp, fuzzing | done | seccomp on in every CI run, child-process filter test; fuzzing below |
| 12 | Tooling and docs: build-guest.sh, setup-tap.sh, tests, README | done | CI builds guest artifacts from scratch and runs everything |

## Fuzzing
Targets in `fuzz/`: `virtqueue`, `virtio_devices` (blk and net through the PCI transport),
`device_registers` (UART, i8042, ACPI PM, PCI config space, virtio BAR), `api_json`,
`bzimage`. Local runs, none of which found a crash:

| Target | Time | Executions |
|---|---|---|
| virtqueue (two input layouts) | 2 x 5 min | 16.1 M + 14.7 M |
| virtio_devices | 5 min + 10 min | 2.7 M + 5.3 M |
| device_registers | 5 min | 2.6 M |
| api_json | 5 min | 28.0 M |
| bzimage | 5 min | 14.8 M |

CI runs every target for 60 seconds on each push. Hardening found by review rather than
fuzzing: empty bus accesses are ignored and restored virtqueues are revalidated.

## Decisions
- ACPI (RSDP/XSDT/FADT/FACS/DSDT/MADT) instead of MP tables: the MADT lists the CPUs and
  the FADT PM1 control block gives the guest an S5 poweroff the VMM can see. The DSDT
  declares the PCI root bridge with `_CRS` windows so Linux keeps our BAR assignments.
- Reboot uses the i8042 pulse-reset command (`reboot=k`); reboot and poweroff both end
  the VMM, like Firecracker.
- MSI-X only (no INTx); routes are programmed with KVM_SET_GSI_ROUTING and delivered
  through irqfds.
- The control API is served from the main epoll loop (no extra thread); JSON is handled
  by a small in-tree codec because serde_json is not on the dependency allow-list. The
  logger is a few lines in-tree instead of another crate.
- vCPU kicks: a real-time signal whose handler sets `kvm_run.immediate_exit` through a
  thread-local pointer, so a kick sent just before `KVM_RUN` is never lost.
- Snapshot file: magic, format version, postcard-encoded state, then raw guest memory
  regions in address order. Virtqueues are kicked once after restore so requests that
  were published before the snapshot are not stranded.
- stdin is read with raw `read(2)`: Rust's buffered `Stdin` hid bytes from epoll and
  stalled console input longer than the 64-byte UART FIFO (found by CI).
