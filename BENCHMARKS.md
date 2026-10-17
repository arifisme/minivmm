# Benchmarks

All numbers below come from one CI run of the `platform` job (commit `Fuzz vsock snapshot
and restore`) and the VMM's own integration tests in the `test` job of the same run. They are
reproducible with the commands at the end. A shared CI runner is noisy, so treat them as
indicative rather than precise.

## Hardware

GitHub Actions `ubuntu-latest` runner (image ubuntu-24.04, kernel 6.17.0-1022-azure):
4 vCPUs (2 cores, 2 threads each) of an AMD EPYC 9V74, 16 GB of memory, KVM exposed through
nested virtualization (AMD-V). The CPU model varies between runs; earlier runs landed on EPYC
Milan hosts.

## Methodology

- The benchmark is `crates/mvd/tests/benchmark.rs` (`benchmarks`, an ignored test). It starts
  a real `mvd` as root, with the jail and cgroups enabled and mvd logging at `debug`, deploys the
  native example (`examples/native`, a static Rust binary) and the python example
  (`examples/python`) with 128 MiB and 1 vCPU, and drives them over HTTP on loopback.
- Every function is invoked with the event `{"action":"echo"}`.
- **Cold starts**: 50 invocations per runtime, each after the previous VM was reaped
  (idle timeout 300 ms), so every one restores the function snapshot. Reported per request:
  client latency, server time (`x-mv-duration-ms`), restore time (`x-mv-restore-ms`: VMM
  spawn until the agent has been reseeded and its clock set) and function time
  (`x-mv-function-ms`, measured by the agent).
- **Warm invocations**: 500 sequential invocations of a warm VM.
- **Throughput**: 1600 requests from 1, 4 and 16 client threads; the pool scales out to the
  concurrency (limit 32 per function).
- **Density**: 200 VMs of 128 MiB are started for one function by holding 200 concurrent
  invocations open, with memory overcommit 4 so admission control admits them. RSS and PSS
  come from each VMM's `/proc/<pid>/smaps_rollup`, the host figure from `MemAvailable`.
- **VMM restore**: `clones_restore_independently_from_one_snapshot` in
  `crates/minivmm/tests/snapshot.rs` restores a 1-vCPU, 128 MiB busybox guest ten times and
  reports the VMM's own restore time (state file read, memory mapped, KVM state set).
- Percentiles are nearest-rank over the samples listed.

## VMM restore

| | min | median | max |
|---|---|---|---|
| minivmm restore (10 runs) | 1.03 ms | 1.23 ms | 1.48 ms |
| inside a cold start (`restore_us` from the VMM API) | | 1.45 to 1.50 ms | |

Guest memory is mapped `MAP_PRIVATE` from the sparse snapshot file, so restore time does not
grow with guest memory size: pages are faulted in from the page cache when touched.

## Cold starts

| Runtime | metric | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| native | client latency | 22.3 ms | 30.9 ms | 35.0 ms | 35.0 ms |
| native | server time | 22.0 ms | 30.6 ms | 34.7 ms | 34.7 ms |
| native | restore | 19.0 ms | 27.5 ms | 31.6 ms | 31.6 ms |
| native | function | 2.55 ms | 2.58 ms | 2.67 ms | 2.67 ms |
| python | client latency | 37.6 ms | 44.5 ms | 48.9 ms | 48.9 ms |
| python | server time | 37.3 ms | 44.2 ms | 48.5 ms | 48.5 ms |
| python | restore | 21.5 ms | 28.2 ms | 32.6 ms | 32.6 ms |
| python | function | 14.4 ms | 15.0 ms | 16.0 ms | 16.0 ms |

Breakdown of the restore phase of a typical native cold start (from mvd's debug log):

| Phase | Time |
|---|---|
| fork and exec `mv-jailer` | 0.4 to 0.6 ms |
| jail setup, VMM start, snapshot restore (1.5 ms of it), vsock socket listening | about 8 to 10 ms |
| vsock handshake with the agent (guest wakes up, handles the transport reset, accepts) | 6 to 8 ms |
| entropy reseed and clock set | 1.2 to 1.3 ms |
| **total** | **16 to 19 ms** |

The function time of a cold start is higher than a warm one (2.6 ms against 0.2 ms for
native, 14 ms against 0.2 ms for python) because the first invocation faults in the guest
pages it touches from the snapshot file; python touches many more.

## Warm invocations

| Runtime | metric | p50 | p90 | p99 | max |
|---|---|---|---|---|---|
| native | client latency | 0.30 ms | 0.46 ms | 0.57 ms | 9.41 ms |
| native | server time | 0.17 ms | 0.32 ms | 0.43 ms | 9.22 ms |
| python | client latency | 0.37 ms | 0.52 ms | 0.64 ms | 4.66 ms |
| python | server time | 0.22 ms | 0.37 ms | 0.48 ms | 4.50 ms |

## Throughput

| Runtime | clients | requests/s | p50 | p99 | max |
|---|---|---|---|---|---|
| native | 1 | 2966 | 0.30 ms | 0.55 ms | 14.4 ms |
| native | 4 | 5396 | 0.59 ms | 1.68 ms | 38.4 ms |
| native | 16 | 4607 | 1.21 ms | 24.5 ms | 148 ms |
| python | 1 | 2722 | 0.33 ms | 0.39 ms | 12.7 ms |
| python | 4 | 4526 | 0.72 ms | 1.70 ms | 65.0 ms |
| python | 16 | 3895 | 0.99 ms | 24.4 ms | 255 ms |

All 1600 requests succeeded in every run. The maxima include the cold starts of the VMs the
pool adds while scaling out. With 16 clients the four host CPUs are shared by the load
generator, mvd and up to 16 VMs, so throughput drops slightly and the tail grows.

## Density and copy-on-write savings

| Runtime | VMs | guest memory promised | PSS total | RSS per VM | PSS per VM | MemAvailable drop per VM | start time |
|---|---|---|---|---|---|---|---|
| native | 200 × 128 MiB | 25600 MiB | 604 MiB | 10.4 MiB | 3.0 MiB | 3.7 MiB | 8.2 s |
| python | 200 × 128 MiB | 25600 MiB | 736 MiB | 19.0 MiB | 3.7 MiB | 3.8 MiB | 8.5 s |

200 VMs promising 25 GiB fit on a 16 GB host using about 0.75 GB. RSS counts the snapshot
pages a VM has touched, which all clones share through the page cache; PSS divides those
shared pages among the clones. The difference between RSS and PSS is the copy-on-write saving:
70 % for native, 80 % for python. The VMM's own code and data are part of every figure.

## Against the targets

- VMM restore in single-digit milliseconds: yes, 1.0 to 1.5 ms.
- Cold starts well under 100 ms: yes, p99 35 ms (native) and 49 ms (python) end to end.
- The largest remaining part of a cold start is outside the VMM's restore: setting up the
  jail and starting the VMM process, then the first vsock handshake while the restored guest
  wakes up and handles the transport reset.

## Reproducing

```
scripts/build-guest.sh
cargo build --release --target x86_64-unknown-linux-musl -p minivmm -p mv-agent -p mv-example-native
sudo AGENT=target/x86_64-unknown-linux-musl/release/mv-agent scripts/build-runtimes.sh
cargo build --release -p mvd -p mvctl && cargo test --release -p mvd --no-run
sudo target/release/deps/benchmark-* --ignored --nocapture --test-threads=1
```

The CI workflow shows the exact steps. Against a running daemon, `mvctl bench NAME
--requests N --concurrency C` is a load generator that reports throughput and cold and warm
latency percentiles.
