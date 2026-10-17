# Progress

minivmm (the VMM) was complete, audited and green on CI before the platform work began; see
AUDIT.md and git history up to `e0f60dc`. This file tracks the serverless platform built on it.

## Environment
- Local sandbox: no `/dev/kvm`. Everything KVM-related is verified on the GitHub Actions
  runner (`ubuntu-latest`, AMD EPYC, 4 vCPUs, 16 GB, KVM enabled through a udev rule). Job
  logs are read with the GitHub MCP `get_job_logs` tool (the REST log download redirects to a
  blocked host). QEMU TCG works locally for checking guest images without KVM.
- Rust 1.97 stable with the musl target (static VMM for the jail, static agent for the guest).
- Terraform is not installed; `terraform fmt -check` and `terraform validate` were run locally
  with a binary and the Google provider from releases.hashicorp.com (registry.terraform.io is
  blocked), and run in CI with `hashicorp/setup-terraform`.
- Force-pushing is not permitted, so a few intermediate commits do not build on their own
  (`bf32a23`, `992c216`); the commit after each fixes it.

## Definition of done
| # | Item | Status | Verified by |
|---|------|--------|-------------|
| 1a | CoW restore: separate page-aligned sparse memory file mapped `MAP_PRIVATE`; restore latency measured | done | unit tests; CI `clones_restore_independently_from_one_snapshot` (median restore 1.4 ms, clone RSS 11 MiB vs PSS 5.2 MiB) |
| 1b | virtio-vsock over modern PCI, host connections via Unix socket | done | 18 unit tests; CI `vsock_carries_data_both_ways` |
| 1c | Clones independent: no NIC, read-only shared rootfs, per-clone API and vsock paths | done | CI `vsock_listeners_survive_restore_in_every_clone` |
| 1d | Clones do not share secrets: entropy reseed and wall clock fix | done | CI `restored_vms_get_fresh_entropy_and_the_current_time` (distinct kernel and python random values in sibling clones, clocks within 3 ms) |
| 1e | Existing tests and fuzz targets pass; vsock fuzz target | done | CI test + fuzz jobs; 5-minute local runs (below) |
| 2a | mv-agent: load, init, invoke, logs, health, reseed/clock over vsock | done | agent tests over a Unix socket; CI end-to-end |
| 2b | native and python runtimes | done | CI end-to-end for both |
| 2c | `scripts/build-runtimes.sh` (busybox native image, debootstrap python image) | done | CI platform job builds both |
| 3 | mvd: API, invoke, pool, failures, scaling, admission, jail, cgroups, persistence, metrics | done | unit and API tests; CI end-to-end suite |
| 4 | mvctl | done | unit tests; CI `the_cli_drives_the_platform` |
| 5 | Benchmarks | in progress | CI benchmark step |
| 6 | Terraform deployment | done | `terraform fmt -check`, `terraform validate` (locally and in CI) |
| 7 | Unit and e2e tests in CI | in progress | CI platform job |
| 8 | Docs (README, ARCHITECTURE.md, BENCHMARKS.md) | in progress | |

## Fuzzing
Local 5-minute runs of every target after the vsock work: api_json 12.1 M executions,
bzimage 23.4 M, device_registers 2.3 M, snapshot_state 7.9 M (remaining targets running).

## Decisions
- Workspace layout: `crates/{minivmm,mv-proto,mv-agent,mvd,mvctl}` plus `examples/native`;
  minivmm keeps its strict dependency list.
- Snapshot format v2: the state file holds the postcard state only; guest memory goes to
  `<state>.mem` (or `mem_path`), written sparse and mapped `MAP_PRIVATE | MAP_NORESERVE` on
  restore, so clones share untouched pages through the page cache.
- vsock follows Firecracker's host model (`CONNECT <port>\n` / `OK <port>\n`; guest
  connections go to `<uds>_<port>`), implemented from the virtio spec: 256 KiB per-connection
  buffers with credit accounting, credit violations reset, replies stop when the guest posts
  no receive buffers, 64 connections at most, `TRANSPORT_RESET` after restore.
- Agent protocol: frames of two little-endian `u32` lengths, a JSON header and a binary
  payload; logs are asynchronous frames. Functions use JSON lines on stdin/stdout; stderr is
  the log stream; the agent drains stderr before handing over a stdout line.
- The agent is PID 1; functions run as root inside their own VM. The VM is the boundary.
- Two-level snapshots: a base per (runtime, memory, vCPUs) built lazily and invalidated by
  kernel/rootfs identity; a function snapshot per deploy revision. Function snapshots record
  the rootfs identity and are rebuilt from stored code on startup if the image changed.
- Jail: `mv-jailer` (part of mvd) joins the VM's cgroup, unshares mount, network, IPC and UTS
  namespaces, bind-mounts the read-only inputs into a per-VM directory, creates a private
  `/dev/kvm` node, pivots into it, drops to a per-function uid with no new privileges and
  execs the static (musl) VMM. The per-VM directory lives on the host so the VMM's sockets and
  snapshot outputs are visible to mvd. No PID namespace, to keep the VMM a direct child.
- cgroups: `<root>/<vm-id>` with `memory.max` (guest + overhead; builds get room for a second
  copy because snapshot page cache is charged to them), `memory.oom.group`, `cpu.max` per
  vCPU and `pids.max`; VMs are killed with `cgroup.kill`.
- Admission: memory reservations per live VM against a host limit (evict oldest idle VMs
  first, then 429), `MemAvailable` floor, vCPU reservations per running invocation.
- Function errors return 500 with `x-mv-function-error`; platform failures 502/504; throttling
  429 with `Retry-After`.
- Logs are in-memory ring buffers per function; metadata in SQLite (WAL, synchronous FULL).
- musl differences found on CI and covered by tests: `open()` uses `SYS_open` and
  `pthread_kill` uses `tkill`, so both are in the VMM filter; a stdin that epoll cannot watch
  (`/dev/null`) is treated as closed.

## Blockers
- None.
