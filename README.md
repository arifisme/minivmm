# minivmm

A Lambda-style serverless platform built on a small KVM virtual machine monitor written from
scratch in Rust. Users deploy functions (static Linux binaries or Python modules) and invoke
them over HTTP. Every invocation runs in its own microVM, restored copy-on-write from a
snapshot taken after the function finished initializing, so a cold start skips both kernel
boot and function init.

- **`minivmm`**: the VMM. KVM, virtio-blk, virtio-net and virtio-vsock over modern PCI,
  snapshot/restore with copy-on-write memory, per-thread seccomp filters.
- **`mvd`**: the platform daemon. HTTP API, per-function warm pools, scale-out and scale to
  zero, admission control, two-level snapshots, a jail per VMM (mount namespace, cgroup v2,
  unprivileged uid), persistence, Prometheus metrics and structured logs.
- **`mv-agent`**: the guest agent, a static musl binary that runs as PID 1 inside each VM and
  talks to the host only over vsock.
- **`mvctl`**: the CLI, including a load generator.
- **`mv-proto`**: the protocol and API types shared by all of them.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design and [BENCHMARKS.md](BENCHMARKS.md) for
measured cold starts, warm latency, throughput and density.

## Repository layout

| Path | Contents |
|---|---|
| `crates/minivmm` | The VMM (library, binary, integration tests) |
| `crates/mvd` | The daemon and `mv-jailer`, unit, API, end-to-end and benchmark tests |
| `crates/mv-agent` | The guest agent and the Python bootstrap |
| `crates/mvctl` | The CLI |
| `crates/mv-proto` | Host-agent frames, the function line protocol, HTTP API types |
| `examples/native`, `examples/python` | Example functions (also used by the tests) |
| `guest/` | Guest kernel config, test rootfs files, the vsock test program |
| `scripts/` | `build-guest.sh` (kernel, test images), `build-runtimes.sh` (runtime images), `setup-tap.sh` |
| `deploy/` | Terraform for a GCP host with nested virtualization |
| `fuzz/` | cargo-fuzz targets |

## Requirements

- x86_64 Linux with KVM (`/dev/kvm`), cgroup v2 with the `cpu`, `memory` and `pids`
  controllers, and root for `mvd` (it creates jails, device nodes and cgroups).
- Rust stable (edition 2024) with the `x86_64-unknown-linux-musl` target.
- For building images: `gcc`, `make`, `flex`, `bison`, `bc`, `libelf-dev`, `libssl-dev`,
  `cpio`, `e2fsprogs`, `debootstrap`, `apt-get` and network access to GitHub and an Ubuntu
  mirror.

## Quick start

```
# Guest kernel (Linux 6.12, tiny config) and the VMM's own test images
scripts/build-guest.sh

# Static binaries for the jail and the guest
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p minivmm -p mv-agent -p mv-example-native
cargo build --release -p mvd -p mvctl

# Runtime root filesystems: guest/out/runtimes/{native,python}.ext4
sudo AGENT=target/x86_64-unknown-linux-musl/release/mv-agent scripts/build-runtimes.sh

# Start the daemon (as root)
sudo target/release/mvd \
    --data-dir /var/lib/mvd \
    --kernel guest/out/bzImage \
    --runtime-dir guest/out/runtimes \
    --vmm target/x86_64-unknown-linux-musl/release/minivmm
```

In another terminal:

```
$ target/release/mvctl deploy hello examples/python --runtime python
deploying hello (1009 bytes of code)...
deployed hello revision 1 (init 31.2 ms)

$ target/release/mvctl invoke hello --data '{"action":"echo","name":"ada"}' --verbose
request 1c4e...  start cold  duration 21.437 ms  function 0.412 ms  restore 18.201 ms  vm hello-3
{
  "echo": { "action": "echo", "name": "ada" },
  "invocations": 1,
  ...
}

$ target/release/mvctl deploy echo target/x86_64-unknown-linux-musl/release/bootstrap --runtime native
$ target/release/mvctl bench echo --requests 2000 --concurrency 8
```

The first deploy for a given runtime, memory size and vCPU count also builds the runtime's
base snapshot (one boot, about a second); later deploys reuse it.

## Writing functions

**Native.** The handler is a static Linux executable in the archive (default name
`bootstrap`). It speaks JSON lines on stdin/stdout:

1. After its own initialization it prints `{"ready":true}`.
2. For every request it reads one line,
   `{"request_id":"...","deadline_ms":1760000000000,"reseed":false,"event":<event>}`,
   and prints one line: `{"response":<json>}` or
   `{"error":{"type":"...","message":"..."}}`.
3. Anything written to stderr is captured as log lines for the current request. `reseed` is
   true on the first request after the VM was restored; reseed any user-space RNG then.

`examples/native` is a complete example in Rust.

**Python.** The handler is `module.function` (default `main.handler`), called as
`handler(event, context)` with the decoded event; it returns any JSON-serializable value. The
module is imported once during init, so module-level work is captured in the snapshot.
`context` has `request_id`, `function_name`, `memory_limit_in_mb`, `deadline_ms` and
`get_remaining_time_in_millis()`. `print` output goes to the function's logs; an exception
becomes a function error with its traceback. `random` is reseeded after every restore.

Functions run as root inside their own VM, with the code unpacked in `/run/mv/function` (a
tmpfs), the runtime image mounted read-only, writable `/tmp`, and no network.

## CLI

```
mvctl [--url URL] [--token TOKEN] <command>      # or MVD_URL / MVD_API_TOKEN

deploy NAME PATH --runtime native|python [--handler H] [--memory MIB] [--vcpus N]
                 [--timeout 3s] [--env KEY=VALUE]... [--max-concurrency N]
invoke NAME [--data JSON | --file PATH|-] [--verbose]
logs NAME [--limit N] [--request-id ID] [--follow]
list
describe NAME
delete NAME
status
bench NAME [--requests N] [--concurrency C] [--data JSON] [--json]
```

`PATH` may be a directory (packed as tar.gz), a `.tar`/`.tar.gz`/`.tgz` archive, or a single
file (packed under the handler name). Errors print the server's message, its error kind and,
for failed deploys, the init logs.

## HTTP API

All endpoints except `/healthz` require `Authorization: Bearer <token>` when `mvd` runs with
`--api-token` (or `MVD_API_TOKEN`). Errors are JSON:
`{"error":"message","kind":"not_found","logs":[...]}`.

| Method and path | Description |
|---|---|
| `PUT /v1/functions/{name}` | Deploy or update. `multipart/form-data` with a `config` field (JSON, below) and a `code` field (tar or tar.gz). Builds the function snapshot; `201` with the function, `422` (`init_failed`, `load_failed`) with init logs, `400` for invalid input |
| `GET /v1/functions` | List functions with their pool sizes |
| `GET /v1/functions/{name}` | Describe a function |
| `DELETE /v1/functions/{name}` | Delete it and stop its VMs (`204`) |
| `GET /v1/functions/{name}/logs?limit=100&request_id=...` | Recent log lines (function stdout/stderr, init output, platform events) |
| `POST /v1/functions/{name}/invoke` | Synchronous invocation; the body is the event (JSON, up to 6 MiB) |
| `GET /v1/status` | Version, uptime, VM counts and host capacity |
| `GET /v1/vms` | Live VMs: function, state, pid, uid, cgroup, jail, invocations, memory (RSS, PSS, cgroup) |
| `GET /metrics` | Prometheus metrics |
| `GET /healthz` | Liveness |

Deploy configuration (all fields but `runtime` optional):

```json
{
  "runtime": "python",
  "handler": "main.handler",
  "memory_mib": 128,
  "vcpus": 1,
  "timeout_ms": 3000,
  "env": {"KEY": "value"},
  "max_concurrency": 10
}
```

Invocation responses carry `x-mv-request-id`, `x-mv-duration-ms` (time in mvd),
`x-mv-function-ms` (time inside the function), `x-mv-start` (`cold` or `warm`),
`x-mv-restore-ms` (cold starts: VMM spawn to synchronized agent) and `x-mv-vm`.

| Status | Kind | Meaning |
|---|---|---|
| 200 | | The function's response |
| 500 | (header `x-mv-function-error: unhandled`) | The function returned an error; the body is `{"type","message","trace"}` |
| 400 | `bad_request` | The event is not valid JSON |
| 404 | `not_found` | No such function |
| 429 | `concurrency`, `memory`, `host_memory`, `cpu` | Throttled; `Retry-After: 1` |
| 502 | `vm_crashed`, `runtime_exited`, `agent_error`, `response_too_large`, `launch_failed` | The VM or runtime failed; only this request is affected |
| 504 | `timeout` | The function exceeded its timeout; its VM was destroyed |

### Metrics

`mvd_invocations_total{function,start}`, `mvd_invocation_errors_total{function,kind}`,
`mvd_invocation_timeouts_total{function}`, `mvd_throttled_total{function,reason}`,
`mvd_invocation_duration_seconds{function,start}`, `mvd_cold_start_seconds{function}`,
`mvd_restore_seconds{runtime}`, `mvd_deployments_total{result}`,
`mvd_pool_vms{function,state}`, `mvd_host_memory_{limit,reserved,available}_bytes`,
`mvd_host_vcpus_busy`, `mvd_host_vcpu_limit`.

Logs are written with `tracing` (`--log-format json` for structured output); every
invocation runs in a span carrying the function and request id.

## Running mvd

| Option | Default | Meaning |
|---|---|---|
| `--listen ADDR` | `127.0.0.1:8080` | HTTP listen address |
| `--data-dir DIR` | `/var/lib/mvd` | Metadata, code, snapshots, jails |
| `--kernel PATH` | | Guest kernel |
| `--runtime-dir DIR` | | `native.ext4` and `python.ext4` |
| `--vmm PATH` | | Static `minivmm` binary |
| `--jailer PATH` | next to `mvd` | `mv-jailer` |
| `--cgroup-root DIR` | `/sys/fs/cgroup/mvd` | Parent cgroup for VMs |
| `--uid-base N` | `900000` | VMMs of function *n* run as uid/gid `N + 1 + n`; builds use `N` |
| `--idle-timeout D` | `5m` | Stop VMs idle this long (scale to zero) |
| `--queue-timeout D` | `2s` | How long a request waits for a VM at max concurrency before 429 |
| `--init-timeout D` | `30s` | Limit for a function's init phase during deploy |
| `--memory-limit-mib N` | host memory minus reserve | Memory VMs may reserve |
| `--memory-reserve-mib N` | `512` | Refuse new VMs when `MemAvailable` drops below this |
| `--vm-overhead-mib N` | `32` | Memory charged per VM beyond guest memory |
| `--memory-overcommit X` | `1.0` | Multiplier on the memory limit (CoW makes real usage much lower) |
| `--vcpu-limit N` | 2 per host CPU | vCPUs that may run invocations at once |
| `--max-code-bytes N` | 64 MiB | Largest code archive |
| `--api-token T` | | Require a bearer token |
| `--log-format text\|json` | `text` | Log output |
| `--no-jail` | | Run VMMs without jail and cgroups (development only) |

Function metadata (SQLite), code and snapshots live in the data directory and survive
restarts. On SIGTERM the daemon finishes in-flight requests, stops every VM and removes its
jails and cgroups; leftovers from a crash are cleaned up on the next start.

## Deployment

`deploy/` holds Terraform for a single GCP VM with nested virtualization (N2 by default),
a dedicated network, firewall rules for the API (port 8080) and optionally SSH, and a startup
script that builds everything from this repository and installs `mvd` as a systemd unit
(`deploy/mvd.service`).

Create `deploy/terraform.tfvars`:

```hcl
project           = "my-project"
api_allowed_cidrs = ["203.0.113.0/24"]
ssh_allowed_cidrs = ["203.0.113.0/24"]
api_token         = "a-long-random-token-of-at-least-24-chars"
```

Then:

```
cd deploy
terraform init
terraform apply
export MVD_URL=$(terraform output -raw api_url) MVD_API_TOKEN=a-long-random-token-of-at-least-24-chars
mvctl status        # ready once the startup build finishes (about 15 minutes)
```

The startup script refuses to continue if `/dev/kvm` is missing. The API is plain HTTP;
restrict `api_allowed_cidrs` or put a TLS proxy in front of it. CI checks the configuration
with `terraform fmt -check` and `terraform validate`; it has not been applied from here.

## Testing

```
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # unit, API, agent and (skipping) KVM tests
sudo target/release/deps/platform-*          # end-to-end, needs KVM, root and the images
sudo target/release/deps/benchmark-* --ignored --nocapture
cd fuzz && cargo +nightly fuzz run vsock     # also: virtqueue, virtio_devices,
                                             # device_registers, api_json, bzimage, snapshot_state
```

KVM tests print a message and pass without doing anything when `/dev/kvm`, root or the
artifacts are missing. The GitHub Actions workflow has four jobs: `test` (fmt, clippy, every
unit test and the VMM's KVM integration tests), `platform` (builds the runtime images and
runs the end-to-end tests and benchmarks as root on the KVM runner), `terraform` and `fuzz`.

The end-to-end suite covers both runtimes, scale-out to the concurrency limit, 429s,
scale to zero and back, timeouts, a guest kernel panic and a function exit mid-invoke, a
VMM killed mid-invoke, a daemon restart, isolation between two functions (uids, cgroups,
jail contents, mounts, network namespace, file ownership), fresh entropy and correct clocks
in sibling clones, admission control with eviction, the CLI, a short load test, and checks
that no VM, jail or cgroup is leaked.

## The VMM

`minivmm` can also be used on its own. It boots an unmodified `bzImage` with multiple vCPUs
or restores a snapshot, with a 16550A console, i8042 reset, ACPI poweroff, PCI with MSI-X,
virtio-blk (optionally read-only), virtio-net on a TAP device and virtio-vsock.

```
target/release/minivmm --kernel guest/out/bzImage \
    --cmdline "console=ttyS0 reboot=k panic=1 root=/dev/vda rw" \
    --cpus 2 --mem 512M --disk guest/out/rootfs.ext4 --tap tap0 \
    --vsock-uds /tmp/vm.vsock --api /tmp/vm.sock
```

| Option | Meaning |
|---|---|
| `--kernel PATH` / `--initrd PATH` / `--cmdline TEXT` | What to boot |
| `--cpus N`, `--mem SIZE` | vCPUs (default 1) and memory (default `512M`) |
| `--disk PATH`, `--disk-read-only` | virtio-blk `/dev/vda` |
| `--tap NAME`, `--mac ADDR` | virtio-net on a TAP device |
| `--vsock-uds PATH`, `--vsock-cid N` | virtio-vsock with host connections on a Unix socket (CID default 3) |
| `--api PATH` | JSON-lines control socket |
| `--restore STATE`, `--restore-memory PATH` | Resume a snapshot (memory defaults to `STATE.mem`); `--vsock-uds` and `--api` may differ per clone |
| `--no-seccomp`, `--log-level LEVEL` | Debugging |

**Control API** (`{"cmd":...}` per line): `status` (with `restore_us` after a restore),
`pause`, `resume`, `snapshot` (`path`, optional `mem_path`), `shutdown`.

**vsock.** A host process connects to the `--vsock-uds` socket and writes
`CONNECT <port>\n`; the VMM answers `OK <host port>\n` once the guest accepts, then the
socket carries the stream. Guest connections to the host (CID 2) port `P` are forwarded to
the Unix socket `<uds>_<P>`. After a restore the guest receives a transport reset, so
listening sockets survive and stale connections are closed; host connections made before
the guest has finished handling the reset are held until it has.

**Snapshots.** The state file holds `MVMSNAP\0`, the format version (2), and the
postcard-encoded machine configuration and device state (vCPU registers, irqchips, clock,
PCI/MSI-X, virtqueues). Guest memory is a separate sparse file, regions back to back,
which a restore maps `MAP_PRIVATE`: clones share every page they do not write, and restoring
a guest takes about 1.5 ms on the CI runner. A restored clone needs only the snapshot files
and, if configured, the (read-only) disk image; no network device is needed.

**Security.** The VMM thread and each vCPU thread install their own seccomp filter (ioctls
restricted per thread, no executable mappings, sockets only of family `AF_UNIX`); every guest
input (descriptors, packets, registers, snapshot state) is bounds-checked, and the device
models, virtqueues, vsock packet handling, API parser and snapshot decoder are fuzzed.
AUDIT.md records an adversarial review of the VMM.

## Limitations

- x86_64 only; guests have no network, and functions run as root inside their VM.
- One request per VM at a time (like Lambda); no asynchronous invocation, no streaming
  responses, events and responses up to 6 MiB.
- The API is plain HTTP with an optional bearer token; there are no users, quotas or TLS.
- Logs are kept in memory (2000 lines per function) and are lost on restart.
- Snapshots replay the CPUID and MSRs of the host that built them; move the data directory
  only between identical hosts. A runtime image change triggers a rebuild of every function
  snapshot on startup.
- VMs of the same function share a uid; isolation between VMs of one function relies on the
  per-VM mount namespace, cgroup and the VMM's seccomp filter.
- Deploys are serialized; the first deploy of a new VM shape also boots a base snapshot.
- Admission control reserves each VM's full memory limit unless `--memory-overcommit` is
  raised; CPU admission counts running invocations, not guest CPU use.
- The VMM keeps the limitations listed in AUDIT.md: no INTx, minimal virtio features,
  block I/O on the event loop thread, reboot ends the process.
