# Progress

minivmm (the VMM) was complete, audited and green on CI before the platform work began; see
AUDIT.md and git history up to `e0f60dc`. This file tracks the serverless platform built on it.

## Environment
- Local sandbox: no `/dev/kvm`. Everything KVM-related is verified on the GitHub Actions
  runner (`ubuntu-latest`, AMD EPYC, KVM enabled through a udev rule). Job logs are read with
  the GitHub MCP `get_job_logs` tool (the REST log download redirects to a blocked host).
- Rust 1.97 stable, musl target installed for static guest/jail binaries.

## Definition of done
| # | Item | Status | Verified by |
|---|------|--------|-------------|
| 1a | CoW restore: separate page-aligned sparse memory file mapped `MAP_PRIVATE`; restore latency measured | done | unit tests; CI `clones_restore_independently_from_one_snapshot` (median restore 1.4 ms, clone RSS 11 MiB vs PSS 5.2 MiB) |
| 1b | virtio-vsock over modern PCI, host connections via Unix socket (`CONNECT <port>`, guest-initiated to `<uds>_<port>`) | done | 18 unit tests; CI `vsock_carries_data_both_ways` (4 MiB echo, guest-to-host, refusals) |
| 1c | Clones independent: no NIC needed, read-only shared rootfs, per-clone API and vsock paths (`--vsock-uds` on restore) | done | CI `vsock_listeners_survive_restore_in_every_clone` (transport reset, two clones) |
| 1d | Clones do not share secrets: entropy reseed (`RNDADDENTROPY` + `RNDRESEEDCRNG`) and wall clock fix by the agent | in progress | agent code done; e2e test pending |
| 1e | Existing tests and fuzz targets pass; vsock fuzz target | done | CI test + fuzz jobs |
| 2a | mv-agent: load, init, invoke, logs, health, reseed/clock over vsock | done (host-side tests) | `crates/mv-agent/tests/agent.rs` over a Unix socket |
| 2b | native and python runtimes | done (host-side tests) | agent tests run a shell function and a python module |
| 2c | `scripts/build-runtimes.sh` | todo | |
| 3 | mvd: API, invoke, pool, failures, scaling, admission, jail, cgroups, persistence, metrics | todo | |
| 4 | mvctl | todo | |
| 5 | Benchmarks | todo | |
| 6 | Terraform deployment | todo | |
| 7 | Unit and e2e tests in CI | in progress | |
| 8 | Docs (README, ARCHITECTURE.md) | todo | |

## Decisions
- Workspace layout: `crates/{minivmm,mv-proto,mv-agent,mvd,mvctl}`; minivmm keeps its strict
  dependency list.
- Snapshot format v2: the state file holds the postcard state only; guest memory goes to
  `<state>.mem` (or `mem_path`), written sparse (all-zero pages become holes) and mapped
  `MAP_PRIVATE | MAP_NORESERVE` on restore, so clones share untouched pages through the page
  cache and only pay for pages they dirty.
- vsock follows Firecracker's host model (`CONNECT <port>\n` / `OK <port>\n` on the Unix
  socket; guest connections go to `<uds>_<port>`), implemented from the virtio spec. Each
  connection advertises a 256 KiB buffer; guests that exceed their credit are reset, replies
  stop being queued when the guest posts no receive buffers, and connections are capped at 64.
  Host connections cannot survive a snapshot, so restore sends `TRANSPORT_RESET`.
- Agent protocol: frames of two little-endian `u32` lengths, a JSON header and a binary
  payload (code archives, events, responses, entropy). Logs are asynchronous frames.
- Functions talk to the agent with a line protocol on stdin/stdout (`{"ready":true}`, then one
  response line per request line); stderr is the log stream. Python uses an embedded
  bootstrap that imports the handler once. The agent drains stderr before handing over a
  stdout line so logs precede the response they belong to.
- The agent is PID 1 in the guest (mounts, reaping); functions run as root inside their own
  VM because the VM is the isolation boundary.
- After restore the first invocation carries `reseed: true` so runtimes can reseed user-space
  generators (the python bootstrap calls `random.seed()`).

## Blockers
- None so far. Force-pushing is not permitted, so one intermediate commit (`bf32a23`) does
  not build on its own; the next commit fixes it.
