# Bramble

A hobby operating system for x86_64 (UEFI via Limine, developed in QEMU),
written in `no_std` Rust, whose kernel keeps all of its state in a single
typed directed graph: processes, memory, devices, capabilities, and wait
queues are nodes and edges, not separate tables.

There is no code yet. Start with the design:

- [`docs/DESIGN.md`](docs/DESIGN.md): the graph representation, the node and
  edge type system, the fast-path analysis and its compromises, what the
  design buys and what it costs, and prior art.
- [`docs/PLAN.md`](docs/PLAN.md): nine implementation phases from
  "boots to a framebuffer" to the v1 goal, each with a visible milestone and
  a stated risk, ordered so the riskiest assumptions are tested first.
