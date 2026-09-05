# Bramble

A hobby operating system for x86_64 (UEFI via Limine, developed in QEMU),
written in `no_std` Rust, whose kernel keeps all of its state in a single
typed directed graph: processes, memory, devices, capabilities, and wait
queues are nodes and edges, not separate tables.

Start here:

- [`docs/DEVLOG.md`](docs/DEVLOG.md): a running log written for someone who has
  never built a kernel. Every entry explains the concepts plainly, how the
  problem is normally solved, what Bramble does instead, and what broke.
- [`docs/DESIGN.md`](docs/DESIGN.md): the graph representation, the node and
  edge type system, the fast-path analysis and its compromises, what the
  design buys and what it costs, and prior art.
- [`docs/PLAN.md`](docs/PLAN.md): nine implementation phases from
  "boots to a framebuffer" to the v1 goal, each with a visible milestone and
  a stated risk, ordered so the riskiest assumptions are tested first.

## v1 is reached

Two userspace processes run preemptively, communicate over an endpoint, and the
entire kernel state can be read out by an unprivileged program, checked, drawn
and diffed on the host. `scripts/check.sh` proves all of it on every run.

## State of the build

| Phase | Milestone | Status |
|---|---|---|
| 0 | Boots to a framebuffer under Limine and OVMF | done |
| 1 | The graph crate, host-tested and benchmarked | done |
| 2 | Frame allocator and the boot graph, inspectable | done |
| 3 | Address spaces, and page tables proven to match the graph | done |
| 4 | Threads, preemption, and the fast-path gate | done |
| 5 | Userspace: ring 3, processes, capabilities | done |
| 6 | IPC over an endpoint, with capability transfer | done |
| 7 | Lifecycle and naming from userspace | done |
| 8 | v1: the inspectable kernel | done |
| 9a | Lazy mapping and the address range index | done |
| 9b | Demand allocation and shared memory | done |

```
cargo ktest             # graph crate tests, on the host
./scripts/build-iso.sh  # build the kernel and a UEFI ISO
./scripts/smoke.sh      # boot headless; writes serial.log, screen.png, graph.png
./scripts/check.sh      # everything that must pass before a commit
```

Once a boot has produced `build/serial.log`:

```
python3 tools/graphdump.py build/serial.log --check   # verify the snapshot offline
python3 tools/graphdump.py build/serial.log --diff    # what changed between two snapshots
python3 tools/graphdump.py build/serial.log --png build/v1.png --hide Named,Maps
python3 tools/graphdump.py build/serial.log --reach Process#0.11 Root#0.1
```

The last one is the take-grant safety question from 1977, asked of a running
kernel: could this process *ever* obtain a capability to that object?

Requires QEMU, OVMF, xorriso, and Graphviz. The Rust toolchain is pinned in
`rust-toolchain.toml`; the kernel needs nightly for `abi_x86_interrupt`, while
`bramble-graph` itself builds on stable.
