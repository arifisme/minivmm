# Audit

Adversarial review of minivmm at `eae618f`. Every source file was read; the guest-facing
device models, the vCPU/main-thread protocol, snapshot parsing and the seccomp filters got
the most attention. Findings are listed most severe first. Severity scale:

- **High**: a guest or a malformed input can corrupt host memory or take over the VMM.
- **Medium**: a guest or host-side input reaches host code it should not, or freezes the
  whole VMM.
- **Low**: narrow races, hardening gaps, conformance bugs whose effect stays inside the
  guest, and tests that cannot fail.

No High findings: every guest-memory access is bounds-checked against a single region,
descriptor chains are copied once and bounded by the queue size, ring placement is
validated before a queue can be enabled (and again on restore), every register file is
indexed through `get`/range checks, and malformed snapshots are rejected before they reach
KVM or the device models.

## Findings

### 1. virtio-net forwards offload metadata that was never negotiated (Medium)

- **Location:** `src/devices/virtio/net.rs`, `Net::transmit` and `Net::next_frame`.
- **What goes wrong:** the device offers no checksum or segmentation features and the TAP
  is opened with offloads disabled, yet the 12-byte `virtio_net_hdr` is copied verbatim in
  both directions. On transmit the guest controls `flags`, `gso_type`, `gso_size`,
  `hdr_len`, `csum_start` and `csum_offset`, and the host kernel acts on them
  (`virtio_net_hdr_to_skb` builds partial-checksum and GSO skbs from them). On receive,
  the TAP sets `VIRTIO_NET_HDR_F_DATA_VALID` for packets the host already validated, which
  the guest must not see because `VIRTIO_NET_F_GUEST_CSUM` was not negotiated
  (virtio 1.x §5.1.6.4.1: the device MUST set `flags` to zero).
- **Trigger:** a guest writes a TX header with `gso_type = VIRTIO_NET_HDR_GSO_TCPV4` or
  `flags = NEEDS_CSUM`; the host kernel's GSO/checksum paths process guest-chosen offsets.
  RX: any forwarded packet with `CHECKSUM_UNNECESSARY` reaches the guest with
  `DATA_VALID` set.

- **Resolution (fixed, `c237870`):** both directions now zero the ten offload bytes of
  the header; received frames keep `num_buffers = 1`. New tests
  `transmit_clears_unnegotiated_offload_fields` and `receive_clears_host_offload_flags`
  failed before the change (the guest's GSO/checksum fields reached the backend and the
  host's `DATA_VALID` reached the guest).

### 2. One API client can freeze the event loop (Medium)

- **Location:** `src/api.rs`, `ApiServer::respond` and `ApiServer::read`.
- **What goes wrong:** `respond` switches the client socket to blocking mode and calls
  `write_all`. A client that sends requests but does not read its responses fills the
  socket buffer and the main thread blocks forever: virtqueue processing, console input,
  TAP traffic, other API clients and SIGINT/SIGTERM handling (signalfd on the same loop)
  all stop. Separately, `read` keeps draining the socket until `EWOULDBLOCK` and only then
  checks `MAX_REQUEST_LEN`, so a client streaming bytes without a newline grows the buffer
  without bound inside one call.
- **Trigger:** a process with access to the API socket pipelines a few thousand
  `{"cmd":"status"}` lines without reading, or streams data with no newline.

- **Resolution (fixed, `2f726dc`):** responses are written without blocking and a client
  whose socket buffer is full is dropped; `read` stops draining once the buffer passes
  `MAX_REQUEST_LEN`. Before the change `client_that_never_reads_cannot_block_the_server`
  timed out with the server blocked in `respond`, and `one_read_buffers_a_bounded_amount`
  saw 438 KB buffered by a single call.

### 3. Kernel footprint and initrd placement ignore the kernel's runtime address (Low)

- **Location:** `src/boot/bzimage.rs` (`load_bzimage`, `LoadedKernel::end`) and
  `src/boot/params.rs` (`initrd_address`).
- **What goes wrong:** the boot protocol defines `init_size` relative to the kernel's
  runtime start: `pref_address` for a non-relocatable kernel, otherwise the load address
  raised to `pref_address` and aligned to `kernel_alignment`. The loader measures it from
  1 MiB instead. With a typical `pref_address` of 16 MiB the computed end is 15 MiB too
  low, so the "kernel too large" check accepts kernels that do not fit and the initrd can
  be placed inside the region the kernel decompresses into, corrupting it during boot.
- **Trigger:** the user boots with little memory or a large initrd, e.g. `--mem 64M` with
  a 40 MiB initrd: the initrd lands at 24 MiB, inside `[16 MiB, 16 MiB + init_size)`.

- **Resolution (fixed, `77eb25b`):** the loader computes the runtime start exactly as the
  boot protocol specifies and uses `max(payload end, runtime start + init_size)` as the
  kernel's end for the fit check and initrd placement; a kernel that would run below
  1 MiB is rejected. `footprint_starts_at_the_runtime_address`,
  `rejects_kernel_whose_runtime_area_does_not_fit` and
  `initrd_stays_clear_of_the_kernel_runtime_area` (the 64 MiB / 40 MiB case above) all
  failed before the change.

### 4. seccomp kills the VMM after a stop/continue during a timed wait (Low)

- **Location:** `src/seccomp.rs`, `COMMON_SYSCALLS`.
- **What goes wrong:** when a thread blocked in a timed `futex`/`clock_nanosleep` is
  stopped and continued without a signal handler running, the kernel restarts the call
  through `restart_syscall` (219), which neither filter allows. The VMM thread waits with
  a timeout in `request_all` (`recv_timeout`) during every pause, resume and snapshot.
- **Trigger:** `kill -STOP`/`kill -CONT`, a debugger attach, or a cgroup freeze/thaw
  (`docker pause`) while an API pause/resume/snapshot is waiting for the vCPUs; the VMM
  exits with "seccomp blocked syscall 219" and the guest is lost. Reproduced in a child
  process with the VMM filter installed.

- **Resolution (fixed, `549b532`):** `restart_syscall` is allowed for both thread kinds.
  `timed_waits_survive_a_stop_and_continue` stops and continues a filtered child blocked
  in a timed futex wait; it exited with 159 ("blocked syscall 219") before the change.

### 5. seccomp allows executable mappings (Low)

- **Location:** `src/seccomp.rs`, `COMMON_SYSCALLS`.
- **What goes wrong:** `mmap` and `mprotect` are allowed with any protection on both
  thread kinds. Nothing in the VMM creates executable memory after the filters are
  installed, so allowing `PROT_EXEC` only helps an attacker who already has a memory
  corruption primitive turn it into code execution.
- **Trigger:** post-exploitation hardening; not reachable without another bug.

- **Resolution (fixed, `976b712`):** `mmap` and `mprotect` are allowed only when
  `PROT_EXEC` is clear. `executable_mappings_are_refused` failed before the change and
  also checks that ordinary read/write mappings still work; the full KVM suite runs with
  the new filters on CI.

### 6. A dying vCPU thread leaves a dangling kick pointer and goes unnoticed (Low)

- **Location:** `src/kvm/vcpu.rs` (`Vcpu::run`), `src/sys/signal.rs` (`KickTarget`),
  `src/vmm/mod.rs` (`spawn_vcpu`).
- **What goes wrong:** `KickTarget::uninstall` only runs at the end of the normal loop.
  If the thread unwinds (a panic in device code running on the vCPU thread), the vCPU fd
  and its `kvm_run` mapping are dropped while the thread-local pointer still refers to
  them, so a kick from `shutdown` or `request_all` makes the signal handler write into
  unmapped (or reused) memory. In both the panic case and the case where installing the
  vCPU seccomp filter fails, the thread exits without telling anyone: the VMM keeps
  running with that vCPU missing (with vCPU 0 the guest never runs) until an API request
  times out.
- **Trigger:** any panic on a vCPU thread, or a kernel that rejects the vCPU filter.

- **Resolution (fixed, `2b0fd6d`, `588d7b3`):** `KickTarget` now detaches the handler in
  `Drop`, and since it is a local of `Vcpu::run` it is dropped before the vCPU (and its
  `kvm_run` mapping) even while unwinding. The vCPU thread holds an `ExitOnPanic` guard
  that requests a `Fault` exit if the thread panics, and a failed filter install also
  requests `Fault`, so the VMM stops instead of running on with a vCPU missing.
  `unwinding_out_of_the_vcpu_loop_uninstalls_the_kick_target` failed before the change
  (the kick wrote into the stale `kvm_run`); `a_panicking_thread_requests_a_fault_exit`
  covers the guard.

### 7. Responses from a timed-out vCPU request can answer a later one (Low)

- **Location:** `src/vmm/mod.rs`, `Vmm::request_all`.
- **What goes wrong:** responses are matched to requests only by count. Stale responses
  are drained before a new request is sent, but one that arrives after the drain is
  accepted as the answer to the new request. A late `State` from an earlier failed
  snapshot can be written into a later snapshot as that vCPU's registers, and a late
  `Paused` can be counted as `Resumed`.
- **Trigger:** a vCPU blocked for more than the 10 s response timeout (for example in a
  console write while the host stops reading stdout), followed by another API request
  whose responses race with the late one.

- **Resolution (fixed, `cd835b1`):** every request carries a sequence number that the
  vCPU echoes; responses with another number are ignored and all vCPUs share one
  deadline. `late_responses_to_earlier_requests_are_ignored` failed against the old
  count-based matching (it returned the stale `Paused`).

### 8. Tests that cannot fail (Low)

- **`devices::virtio::block::tests::read_only_disk_refuses_writes`**
  (`src/devices/virtio/block.rs`): checks that sector 0 is still zero after the refused
  write, but the guest buffer it "writes" is also all zeroes, so the check passes even if
  the device writes the data.
- **`guest_reboot_stops_the_vmm`** (`tests/boot.rs`): asserts the output contains
  `Reboot`, but a triple fault (`KVM_EXIT_SHUTDOWN`) is reported as the same exit reason.
  Linux falls back to a triple fault when the keyboard-controller reset does nothing, so
  the test passes with a broken i8042 reset path.

- **Resolution (fixed, `96e6d6c`, `156ce76`):** the block test now fills the buffer with
  `0xab` first; a mutant that writes the data but still reports `IOERR` passed the old
  test and fails the new one. The i8042 reset path logs "guest reset the machine through
  the i8042", triple faults log a warning, and the reboot test requires the former and
  rejects the latter.

### 9. Corrupted snapshots are neither tested nor fuzzed (Low)

- **Location:** `src/snapshot.rs`, `fuzz/`.
- **What goes wrong:** the requirement that a corrupted snapshot produces an error rather
  than a panic was unverified: `snapshot.rs` has no tests and no fuzz target reaches
  `SnapshotReader` or the device `restore` paths. Review found no panic (kvm-bindings'
  serde impls pad or truncate byte blobs, postcard bounds every allocation by the input,
  device restores validate lengths and queue placement), so this is a coverage gap rather
  than a defect.

- **Resolution (`6778ae7`):** `snapshot.rs` now has a round-trip test and tests that every
  truncation of a snapshot file is an error, that bad magic, version, oversized state
  length and trailing bytes are rejected, and that flipping any byte of the state blob
  never panics. A new fuzz target, `snapshot_state`, decodes arbitrary `VmSnapshot` and
  `VirtioPciState` blobs, restores them into the UART, i8042, ACPI PM, PCI root and a
  virtio-blk device and then drives those devices. All passed without code changes,
  confirming the review.

## Accepted

These are real but deliberately left as they are, for the reason given with each.

- **Guest memory is copied with plain (non-volatile) copies while vCPUs run.** Strictly a
  data race in Rust's memory model. Every guest structure (descriptor, ring index, request
  header, frame) is copied exactly once into host-owned memory and only the copy is used,
  so there is no double fetch for the compiler to introduce or for the guest to exploit.
  Switching to volatile word-by-word copies would cost throughput for no observable gain.
- **String port I/O (`rep ins`/`rep outs`) is delivered as one wide access.** kvm-ioctls
  passes `size * count` bytes without the access size, and the bus drops accesses wider
  than a device's range. The effect stays inside the guest and Linux never uses string I/O
  on these ports.
- **Moving a BAR on top of another device leaves the PCI config, ioeventfds and MMIO bus
  disagreeing** until the guest moves it again (which heals it). The guest can only break
  its own device; the host is unaffected.
- **A guest can keep the event-loop thread busy** by refilling a queue as fast as it is
  drained or by submitting multi-gigabyte block requests (whose used length is then
  truncated to 32 bits). Device emulation is synchronous on one thread by design (README
  limitations); a guest gains nothing it could not get by spinning a vCPU.
- **Console output blocks when the host stops reading stdout.** The vCPU writing to the
  UART blocks while holding the UART lock and the main thread then blocks delivering
  console input. Making the console lossy or buffering without bound are both worse
  defaults for an interactive console.
- **Interrupts still queued in KVM's irqfd workqueue when vCPU state is saved can be
  missing from a snapshot** (the UART's IOAPIC line always takes this path; MSI only when
  KVM's fast path fails). The window is microseconds and cannot be forced from userspace;
  KVM exposes no way to flush it.
- **The VMM thread's filter allows `openat`, `rename` and `unlink` on any path,** and
  accepts unlimited API connections. The snapshot command takes an arbitrary path, and
  the API is trusted (README: protected only by the socket's permissions).

## Verification

- Each fix landed in its own commit with the tests named above; `cargo fmt --check` and
  `cargo clippy --all-targets -- -D warnings` are clean at every commit.
- The CI workflow (KVM enabled on the runner, guest booted from the built kernel,
  initramfs and ext4 root, `tap0` for networking, seccomp on) passed on the fixed code,
  including the strengthened reboot test, which shows the guest now provably resets
  through the i8042.
- After the fixes every fuzz target, including the new `snapshot_state`, ran for 5
  minutes locally without a crash: virtqueue 15.3 M executions, virtio_devices 2.7 M,
  device_registers 2.7 M, api_json 15.5 M, bzimage 29.3 M, snapshot_state 11.1 M. CI
  also fuzzes every target for 60 seconds on each push.

# Platform audit

Adversarial review of the serverless platform (`mvd`, `mv-jailer`, `mv-agent`, `mv-proto`,
`mvctl`, the Terraform deployment and CI) at `5779c96`, plus the VMM changes made for it
(vsock, copy-on-write snapshots). Every source file was read. The threat model:

- **Guests are hostile.** Function code runs as root in its VM and can replace the agent or
  the guest kernel, so everything mvd reads from a guest (frames, log lines, replies) is
  untrusted input.
- **A VMM may be compromised** by a guest escape. The jail is meant to contain it: it must not
  reach other functions' VMs or files, or the host as root.
- **API clients are trusted** to run code (anyone with the token can deploy), but must not be
  able to break the daemon by disconnecting, racing deploys or sending large requests.

Severity uses the scale above. No High findings in the guest-facing paths: frame and reply
sizes are bounded before allocation, archives are extracted with path, type, count and size
limits, and vsock and snapshot parsing were already fuzzed.

## Findings

### 1. The daemon followed symlinks in VMM-writable jail directories (High)

- **Location:** `crates/mvd/src/builder.rs` (`collect`, then `chmod` of the snapshot files),
  `crates/mvd/src/agent.rs` and `vm.rs` (connecting to `v.sock` and `api.sock`).
- **What goes wrong:** each jail directory is owned by the VMM's uid, and the daemon (root)
  renamed `out.state`/`out.mem` out of it and then changed their mode through the path. A
  compromised build VMM could make `out.state` a symlink to any host file and have root
  make it world-readable (base snapshots got mode 0444), or have it bound into later VMs. It
  could also replace its sockets with symlinks to another VM's sockets, so the daemon would
  send one function's events to another function's VM.
- **Resolution (fixed, `e8108b6`):** sockets are opened with `O_PATH | O_NOFOLLOW`, checked to
  be sockets and connected to through `/proc/self/fd`; snapshot files are moved out first
  (so the VMM can no longer swap them), then opened with `O_NOFOLLOW`, required to be plain
  files with one link, and given to root with `fchown`/`fchmod` (group = the function's uid,
  mode 0440; base snapshots 0444). Test `sockets_and_outputs_must_not_be_symlinks`.

### 2. A client disconnect leaked pool slots (High)

- **Location:** `crates/mvd/src/api.rs` handlers, `crates/mvd/src/pool.rs`.
- **What goes wrong:** hyper drops a handler's future when its connection closes. An
  invocation dropped while it held a VM left the pool's busy or starting count raised for
  good (and a stale entry in `/v1/vms`); after `max_concurrency` disconnects the function
  answered 429 forever. Deploys and deletes dropped halfway behaved similarly.
- **Resolution (fixed, `015d783`):** invocations, deploys and deletes run on their own task
  and finish regardless of the client. New end-to-end test
  `abandoned_requests_still_release_their_vms` (four requests abandoned after 300 ms against
  `max_concurrency` 2, then the pool must drain and serve a warm request).

### 3. Queued requests held host CPU reservations (Medium)

- **Location:** `Platform::serve`.
- **What goes wrong:** the vCPU reservation was taken before waiting for a pool slot, so a
  burst queued on one function (up to `--queue-timeout`) held CPU admission and made every
  other function answer 429 "every host CPU is busy".
- **Resolution (fixed, `b0d9c93`):** the reservation is taken after the request gets a VM
  slot; a refused request hands its slot back.

### 4. A deploy removed the previous revision under in-flight cold starts (Medium)

- **Location:** `Platform::deploy`, `remove_old_revisions`.
- **What goes wrong:** the replaced revision's directory was deleted as soon as the new one
  went live, while requests that had already picked the old revision could still be about to
  spawn a VM from it; the jailer then failed to bind the missing snapshot and the request got
  a 502.
- **Resolution (fixed, `16d86da`):** a replaced revision is marked retired and removed when
  its last reference drops; stale revision directories are cleaned on deploy and on startup.
  The Python end-to-end test checks that only the live revision remains.

### 5. API tokens were compared in variable time; an empty token was accepted (Medium)

- **Resolution (fixed, `2b9ac16`, `79ce149`):** tokens are compared through SHA-256 digests
  without data-dependent branches. Startup refuses an empty or non-printable token, a uid base
  below 1000 (0 would have run base-snapshot builds as root) or one that leaves no room for
  function uids, and a non-positive or NaN `--memory-overcommit`; it warns when listening on
  a non-loopback address without a token. Function ids above 2^24 are refused so uids stay in
  range. Test `rejects_unsafe_options`.

### 6. Guests controlled log attribution and size (Medium)

- **Location:** `Platform::serve` log callback, `crates/mvd/src/logs.rs`.
- **What goes wrong:** the request id of each log line came from the guest, so a function
  could file lines under other requests; and the agent's 16 KiB line limit was the only bound,
  so a modified guest could store 2000 lines of 64 KiB per function in the daemon.
- **Resolution (fixed, `ec94080`):** the daemon tags lines with the request it is serving
  (keeping only whether the guest sent them during a request), truncates them to 16 KiB and
  keeps at most 4 MiB per function. Test `bounds_line_length_and_total_bytes`.

### 7. A late function response could answer the next request (Medium)

- **Location:** `crates/mv-agent/src/runtime.rs`, `server.rs`.
- **What goes wrong:** the agent read function stdout continuously into a queue and only
  consumed it during an invocation, so a second response line (a function that answers
  twice, or a background thread) was handed to the next request, possibly from another end
  user, and stray output was logged under the wrong request.
- **Resolution (fixed, `cad7ffb`):** output queued between invocations is dropped before the
  next request is sent (non-protocol lines are kept as logs, protocol lines reported). Test
  `late_responses_never_reach_the_next_request` fails without the change.

### 8. Jail hardening gaps (Low)

- **VMM threads could send any signal** (`tgkill`, `tkill` unrestricted), so a compromised
  VMM could kill the other VMMs of its function, which share its uid. Both are now limited to
  the vCPU kick signal (`3f7e891`, test `threads_can_only_send_the_kick_signal`, run for
  glibc and musl).
- **The jailer kept inherited descriptors and set no file size limit.** It now closes every
  descriptor above standard error and limits files the VMM writes to its memory limit
  (`3c83e0e`).
- **VMMs were killed by pid** after checking that they had not exited, which could signal a
  reused pid in unjailed mode; they are now killed through the child handle (`b856ef9`).

### 9. Smaller defects (Low)

- VMM control replies were accepted when the text `"ok":true` appeared anywhere, including
  inside an error message; they are now parsed (`a53f663`, test
  `control_replies_must_report_ok`).
- The agent, which is PID 1, exited (panicking the guest) on any `accept` error; transient
  errors are now retried (`eb81bc8`).
- Metric series of deleted functions stayed forever (`5f1b690`).
- `mvctl` recursed forever on a symlink loop when packaging a directory, and gave up on
  invocations after 10 minutes although functions may run for 15 (`068545a`).
- The Terraform deployment put the API token in instance metadata (readable by anyone with
  `compute.instances.get`) and pasted it unescaped into a shell script. It is now stored in
  Secret Manager, readable only by the VM's service account, fetched at every boot, and
  restricted to a header-safe character set (`bdbb4a2`).
- The CI token had the default write permissions; it is now read-only (`82df244`).

## Accepted

- **No disk quotas.** Code archives and base snapshots of VM shapes nobody uses any more
  stay on disk; anyone with the token can fill it. Documented in the README.
- **Build VMs are not counted by admission control.** Deploys are serialized, so at most
  one build VM (plus a base build) runs at a time.
- **VMs of one function share a uid.** With the signal restriction above, a compromised VMM
  can no longer signal its siblings; ptrace and `/proc` are unavailable to it.
- **A metric series can reappear** when an invocation finishes just after its function was
  deleted.
- **The token is in the Terraform state**, which must therefore stay private.

## Verification

- `cargo audit` reports no advisories for the workspace or the fuzz lockfile.
- `cargo fmt --check` and `cargo clippy --workspace --all-targets -- -D warnings` (Rust
  1.99) are clean; every unit, API and agent test passes; the end-to-end suite and
  benchmarks pass on the CI KVM runner; `terraform fmt -check` and `terraform validate`
  pass.
- The VMM's device code did not change in this round; its fuzz targets last ran for 5
  minutes each after the vsock and snapshot changes (PROGRESS.md) without a crash.
