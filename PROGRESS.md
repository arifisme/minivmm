# Progress

## Environment
- Local sandbox: no `/dev/kvm` and no vmx/svm CPU flag, so KVM cannot run here.
- End-to-end verification runs on GitHub Actions (`ubuntu-latest`, AMD EPYC host with
  SVM, KVM enabled through the udev rule). Logs are read through the Actions REST API.
- Guest artifacts (kernel v6.12 from the GitHub mirror, Ubuntu `busybox-static`,
  dropbear built from GitHub) build both locally and in CI.

## Definition of done
| # | Item | Status |
|---|------|--------|
| 1 | KVM core | in progress |
| 2 | Boot (bzImage, zero page, e820, cmdline, initrd) | done, boots on CI |
| 3 | SMP + clean shutdown | in progress |
| 4 | Devices, bus, UART, i8042, irqfd/ioeventfd, epoll | in progress |
| 5 | PCI | in progress |
| 6 | virtio 1.x modern PCI + MSI-X | in progress |
| 7 | virtio-blk | in progress |
| 8 | virtio-net | in progress |
| 9 | Control API | in progress |
| 10 | Snapshot/restore | in progress |
| 11 | Hardening (seccomp, fuzzing) | todo |
| 12 | Tooling and docs | in progress |

## Decisions
- ACPI (RSDP/XSDT/FADT/FACS/DSDT/MADT) instead of MP tables: MADT lists the CPUs and
  the FADT PM1 control block gives the guest a way to power off (S5) that the VMM sees.
- Reboot goes through the i8042 pulse-reset command (`reboot=k`); reboot and poweroff
  both stop the VMM, like Firecracker.
- MSI-X only (no INTx fallback); routes are programmed with KVM_SET_GSI_ROUTING and
  delivered through irqfds.
- The control API is served from the main epoll loop (no extra thread); JSON is handled
  by a small in-tree codec because serde_json is not on the dependency allow-list.
- Snapshot file: magic, format version, postcard-encoded device/vCPU/VM state, then raw
  guest memory regions in address order.
