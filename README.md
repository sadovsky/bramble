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

## State of the build

| Phase | Milestone | Status |
|---|---|---|
| 0 | Boots to a framebuffer under Limine and OVMF | done |
| 1 | The graph crate, host-tested and benchmarked | done |
| 2 | Frame allocator and the boot graph, inspectable | done |
| 3 | Address spaces, and page tables proven to match the graph | done |
| 4 | Threads and preemption | next |
| 5-8 | Userspace, IPC, lifecycle, v1 | planned |

```
cargo ktest             # graph crate tests, on the host
./scripts/build-iso.sh  # build the kernel and a UEFI ISO
./scripts/smoke.sh      # boot headless; writes serial.log, screen.png, graph.png
./scripts/check.sh      # everything that must pass before a commit
```

Requires QEMU, OVMF, xorriso, and Graphviz. The Rust toolchain is pinned in
`rust-toolchain.toml`; the kernel needs nightly for `abi_x86_interrupt`, while
`bramble-graph` itself builds on stable.
