# Bramble: Staged Implementation Plan

Companion to `docs/DESIGN.md`. Nine phases from a blank repo to the v1 goal:
two userspace processes running preemptively, communicating over an IPC
edge, with the entire kernel state inspectable as a graph via a syscall.

## Ordering principle

The riskiest assumption is that the graph representation is cheap enough on
the context-switch and IPC paths. The second riskiest is that derived
indices (page tables above all) can be kept consistent with edges. The plan
therefore:

- builds and **benchmarks the graph on the host in phase 1**, before any
  kernel code depends on it. The primitive costs (link, unlink, lookup)
  predict the in-kernel numbers because a context switch's other costs
  (register save, CR3 load) are fixed and known;
- ships the **checker in phase 1** and the first derived index (page tables)
  in phase 3, so no index ever exists without its auditor;
- takes the first **in-kernel fast-path measurement in phase 4**, with a
  named fallback if it fails;
- takes the **IPC measurement in phase 6**, the last point at which the
  representation could still be swapped without rewriting userspace.

Every phase has a milestone you can watch in QEMU or in a terminal, and a
go/no-go where one applies. Phases are sized for roughly one to three
focused weekends each; take the ordering seriously and the estimates lightly.

## Repository layout (established in phase 0/1)

```
bramble/
  Cargo.toml                 workspace
  kernel/                    the kernel binary, x86_64-unknown-none
  graph/                     bramble-graph: no_std, host-testable, no kernel deps
  user/                      bramble-user: syscall wrappers + the v1 programs
  tools/                     host tools: snapshot decoder, DOT renderer, checker
  limine.conf, scripts/qemu.sh, scripts/bench.sh
  docs/
```

`graph/` never depends on `kernel/`. The kernel calls into `graph/` through
a small trait for the platform-specific hooks (write a PTE, flush TLB) so
the checker's page-table cross-check can be mocked on the host.

---

## Phase 0: boots to a framebuffer

**Build.** Rust nightly, `x86_64-unknown-none` target, Limine boot protocol
via the `limine` crate. GDT, IDT with a panic-printing exception handler,
TSS, serial output over COM1, framebuffer text output. A `qemu.sh` that boots
the image with serial on stdio, and a `-enable-kvm` variant.

**Milestone.** "Bramble" drawn on the QEMU framebuffer; the same line plus
the Limine memory map on the serial console; a deliberate `ud2` prints a
register dump instead of triple-faulting.

**Risk.** Toolchain churn (Limine protocol revisions, nightly features for
`naked` functions and the custom target). Low impact, but it is where hobby
kernels most often die of boredom, so keep it to one sitting.

---

## Phase 1: the graph crate, on the host

**Build.** `bramble-graph` as a `no_std` library with `std` enabled only for
tests: slabs, generational IDs, orthogonal edge lists, the `EdgeKind` trait
with the v1 compatibility table, `create_node`/`link`/`unlink`/
`retarget_src`/`move_to_tail`/two-phase `delete_node`, and the **checker**
verifying invariants I1 to I4, I6, I8, I9 (everything that does not need
hardware). Property tests: random operation sequences with the checker run
after every step. Fuzz the ID validation: stale IDs must always fail.
Criterion benchmarks for each primitive.

**Milestone.** `cargo test -p bramble-graph` green with the property suite;
`cargo bench` prints per-op costs.

**Go/no-go.** `link`/`unlink`/`lookup` each under ~30 ns on the host with a
warm cache, and under ~150 ns cold. If cold `lookup` is materially worse than
a pointer chase (it should be one extra compare), the layout is wrong; fix
it here, where fixing it is a refactor rather than a rewrite.

**Risk.** The representation is wrong in a way the host cannot show
(interrupt-context latency, cache behaviour under real workloads). Mitigated
by phases 4 and 6; accepted otherwise.

---

## Phase 2: physical memory and the graph in the kernel

**Build.** Frame bitmap allocator over the Limine memory map. The static
`GRAPH` in `.bss`, `Root` and `Cpu0` nodes, Root-owned `MemoryObject` nodes
for the kernel image, framebuffer, bitmap, and each boot module. A text
dump of the graph over serial (`node kind id` / `edge kind src dst`), which
is the inspect syscall's format minus the syscall. The in-kernel checker
runs at the end of boot.

**Milestone.** Boot prints the graph: `Root Owns Cpu0`, `Root Owns
MemoryObject(kernel_image)`, and so on; the checker reports clean; the
serial dump round-trips through a host tool into a Graphviz picture.

**Risk.** Bootstrapping order: something needs a frame before the bitmap
exists, or a node before the graph is declared. Both are visible in this
phase and cheap to fix. Arena sizing is checked against the memory map here.

---

## Phase 3: address spaces, `Maps` edges, and the page-table invariant

**Build.** The kernel's own page tables (replace Limine's), the kernel
`AddressSpace` node with its two fixed `Maps` edges, `link::<Maps>` and
`unlink::<Maps>` as the only writers of user PTEs, TLB shootdown (local only
in v1), and the checker's page-table walk implementing I5 and I7 on user
spaces. Create a throwaway user `AddressSpace`, map a `MemoryObject` into
it, unmap it.

**Milestone.** Map, checker clean; corrupt a PTE by hand from a debug hook,
checker panics naming the space, the edge, and the mismatched PTE. Unmap,
checker clean. This is the first proof that a derived index can be kept
honest.

**Risk.** I5 is hard to hold across TLB corners (stale entries after an
unmap look like the invariant holding when it does not). Local-only flushes
in v1 keep it tractable; the checker cannot see the TLB, so a stale-TLB test
(write after unmap must fault) is added to the milestone.

---

## Phase 4: threads, preemption, and the first fast-path measurement

**Build.** `Thread` nodes with kernel stacks, `InSpace` edges with the `cr3`
hot-hop cache, the `Cpu`'s `Ready` list, `set_state`, the context switch
with lock hand-off, the LAPIC timer calibrated against the PIT, and the
reaper thread. Two kernel threads alternate on timer ticks. A benchmark
mode: each thread yields to the other N times and the kernel prints the
mean cycles per switch from `rdtsc`. A control build with a hand-rolled
intrusive list scheduler (candidate B for this one edge kind) for the ratio.

**Milestone.** Serial shows the two threads interleaving on ticks, the
checker verifying I6 every tick in debug builds, and a table of switch cost
for the graph scheduler versus the control.

**Go/no-go.** Graph scheduler within 1.5× of the control under KVM. If it
fails: the fallback is to hoist `Ready` into a field-based list on `Cpu`
and `Thread` while still emitting it as edges in inspect (candidate B for
that edge kind only). That fallback is small and does not touch the thesis
elsewhere; decide it here, not in phase 8.

**Risk.** This is the point where the thesis is first tested against
hardware. Also the usual: stack switching bugs that show up as triple
faults; QEMU TCG making the numbers noise (use KVM).

---

## Phase 5: userspace

**Build.** Ring 3 entry and return, `SYSCALL`/`SYSRET` with `swapgs` and a
per-CPU kernel stack, `Process` nodes with the handle table, `Holds` edges
and rights checks, a static ELF64 loader from a Limine module, the
`Device` node for the serial console, and the syscalls `exit`, `yield`,
`write`, `lookup`, `inspect`. The `bramble-user` crate. One program: print
hello via a console capability, then dump the graph via `inspect` and print
it as text.

**Milestone.** A user program prints "hello from ring 3" through a
capability; the same program built without the `write` right gets an error
and exits; a user page fault kills the process and the reaper cleans up
with the checker clean afterwards. The user program's own `inspect` output
matches the kernel's serial dump.

**Risk.** ABI plumbing (user stack alignment, `swapgs` on every path
including exceptions in user mode, `sysret` canonical-address trap). Not a
design risk, just the phase most likely to eat a weekend on one bug.

---

## Phase 6: IPC, and the second fast-path measurement

**Build.** `Endpoint` nodes, `Waiting` edges with role and FIFO order,
`send`/`recv`/`call` with synchronous rendezvous, direct switch to the
partner, capability transfer with rights masking and slot allocation,
`endpoint_create`. Two user programs loaded as two modules, each given an
endpoint capability by the kernel at boot (spawn from userspace arrives in
phase 7). A ping-pong benchmark: one million round trips, cycles per round
trip printed.

**Milestone.** Two processes ping-pong over an endpoint; `inspect` shows the
`Waiting` edge flipping between them; the checker runs clean between
batches; a round-trip number.

**Go/no-go.** Under KVM, a round trip of a few thousand cycles is expected
for a first cut; over ten thousand means the graph ops are not the problem
and something else is (usually a redundant CR3 load or a TLB flush on the
switch path), and profiling continues. If the graph ops themselves show up
as more than ~15 percent of the round trip in a cycle breakdown, apply the
phase 4 fallback to the `Waiting` edge kind as well.

**Risk.** Capability-transfer semantics: the slot reserved by the receiver
before blocking, rights masking, and what happens when the sender dies
mid-rendezvous (the `DYING` check on the far side handles it, but it needs a
test). Also the last point where the representation could be changed
without rewriting user programs; after this phase the design is committed.

---

## Phase 7: lifecycle, naming, and cleanup

**Build.** `spawn`/`start`/`grant`/`mem_create`/`map` from userspace, so
init creates the two workers itself. Process exit with full two-phase
deletion through the reaper. `Named` edges from `Root`, `lookup`. Revocation
tests: kill a process holding the endpoint and confirm the partner's next
`send` returns an error rather than deadlocking; kill a parent and confirm
the child subtree is gone; check the slab occupancy returns to the pre-spawn
count.

**Milestone.** init spawns two workers, hands each an endpoint, they
ping-pong, init kills one, the other's `send` fails cleanly, init respawns
it, and a snapshot after everything exits is identical (modulo generations)
to a snapshot from before. Occupancy counts prove no leak.

**Risk.** Unbounded work under the lock, which the reaper design addresses
but which needs a test with a process owning thousands of objects, plus
cascading deletion ordering bugs (child freed before parent's edge to it is
unlinked). The checker catches the second class; a lock-hold-time histogram
catches the first.

---

## Phase 8: v1, the inspectable kernel

**Build.** The final `inspect` snapshot format (header, node table, sorted
edge table with offsets, virtual edges). The host tools: decode, render to
DOT, diff two snapshots, run the checker's invariants offline, and answer
the take-grant "could A ever obtain X" query. A user-triggered debug syscall
that runs the in-kernel checker on demand.

**Milestone (v1).** With init and two workers ping-ponging under preemption,
a user program takes a snapshot; on the host it renders as a graph showing
`Root`, `Cpu0` with its `Ready` list, three processes with their `Owns` and
`Holds` edges, four address spaces with `Maps` edges, the endpoint with its
`Waiting` edge, and the console device with its `Named` edge. The offline
checker agrees with the in-kernel one. Two snapshots a second apart diff to
exactly the `Ready`/`Waiting` flips.

**Risk.** Snapshot consistency under the lock (hold time grows with graph
size) and a user buffer that is too small. Both are engineering, not design.

---

## Phase 9 (post-v1 candidates, in suggested order)

Not part of v1. Listed so the v1 design does not paint itself out of them.

1. **Lazy mapping and the per-space range index**, which also enables
   demand-zero memory and copy-on-write `MemoryObject`s (a `Maps` edge
   attribute plus a fault handler that consults the index, never the graph).
2. **Async notifications** as a bitmask on `Endpoint`, so an interrupt can
   signal a userspace driver without a blocked receiver.
3. **Chunked slab growth** from the frame allocator.
4. **SMP**: `Cpu` nodes with their own locks, cross-CPU TLB shootdown, thread
   migration as `retarget_src` on a `Ready` edge, and the global lock
   retreating to structural mutations. The lock audit from design section
   3.8 is the entry criterion.
5. **Reparenting and orphan semantics** as a distinct edge kind if `Owns` as
   parentage proves too rigid.
6. **Persistence**, only after a separate design document. Do not start it
   as a weekend project.

---

## Build log

Recorded as phases land, so the go/no-go gates have real numbers attached.
`docs/DEVLOG.md` is the companion narrative: the same phases explained from
first principles, with the concepts, the conventional approach, and what broke.

### Phase 0: boots to a framebuffer — **done**

Boots under OVMF in QEMU via Limine 9.x, UEFI only. Serial on COM1 at 115200,
a scrolling framebuffer console with an 8x16 font baked from DejaVu Sans Mono
by `tools/genfont.py`, and GDT, TSS and IDT with handlers for the faults a
young kernel actually hits.

All three legs of the milestone are demonstrated by `scripts/smoke.sh`, which
boots headless, captures the serial log, takes a framebuffer screenshot over
QMP, and fails the build if the expected line never appears:

- the wordmark and console text render (`build/screen.png`);
- the Limine memory map reaches the serial log (31 entries, 462 MiB usable);
- `scripts/smoke.sh --cmdline faulttest` executes `ud2` and gets a register
  dump from the `#UD` handler rather than a triple fault.

Two notes for later phases. There is **no KVM** in the development container,
so everything runs under TCG and the phase 4 and 6 timing gates have to be read
as ratios against an in-kernel control, never as absolute cycle counts
(DESIGN Q8). And the `limine` crate tracks protocol revision 9, so the
bootloader binaries must come from the `v9.x-binary` branch; booting 8.x
silently drops the renamed requests rather than failing loudly, which cost an
hour of confusion over an empty kernel command line.

**Risk retired:** toolchain and boot plumbing. The kernel needs nightly only
for `abi_x86_interrupt`; `bramble-graph` still builds on stable.

### Phase 1: the graph crate, on the host — **done**

`cargo test -p bramble-graph` runs 24 tests including two randomised property
suites that call the checker after every operation. `bramble-graph` is
`no_std`, `forbid(unsafe_code)`, and allocates nothing.

Measured on the host in release (`cargo test --release -- --nocapture bench`):

| Operation | ns/op |
|---|---|
| `resolve` (handle to object, with rights check) | 5.5 |
| header lookup (id to node) | 3.1 |
| `pick_next` (run-queue head) | 3.1 |
| `rotate_ready` (round robin) | 6.9 |
| `link` + `unlink` pair | 49.9 |
| full checker, 69 nodes / 261 edges | 8130 |

Fast path against graph size: 8.76 ns on a 2-thread graph, 8.98 ns on a
100-thread, 200-capability graph. A ratio of 1.03, which is the property that
actually matters.

Static footprint: `Edge` 64 bytes, `NodeHeader` 80, `NodeId` 8, whole `Graph`
429 KiB of `.bss`.

**Verdict: go.** Every primitive is single-digit nanoseconds and none scales
with graph size.

**What the checker caught, in its first three runs.** All three are the bug
class DESIGN 5.3 predicted would dominate: a derived index diverging from the
edge it shadows.

1. **A stale `cr3` cache.** Nothing enforced that a thread has at most one
   `InSpace` edge, so a second one left the hot-hop cache pointing at the first
   address space. Fixed by making `InSpace` replace rather than accumulate, and
   by adding cardinality checks for every single-valued relationship.
2. **A thread blocked forever on a destroyed endpoint.** Reaping an endpoint
   unlinked its `Waiting` in-edges and left the thread `Blocked` with no edge.
   This is the deadlock the phase 7 milestone was meant to find, surfacing six
   phases early. Fixed with explicit abort semantics: the reaper reports the
   stranded thread and the kernel resumes it with an error.
3. **A half-applied state transition.** `make_blocked` unlinked the thread's
   run-queue edge and then failed to link the wait edge because the endpoint was
   already dying, leaving the thread `Ready` with nothing to run it. Fixed with
   `precheck_link`, so every multi-step transition validates before it mutates.

The lesson is the one the design predicted, and it is worth restating: the
graph's own structure was never the problem. Every bug was a cache, and the
checker found all three within seconds of existing.

### Phase 2: physical memory and the graph in the kernel — **done**

A bitmap frame allocator over the Limine memory map, carving its own storage
out of the first usable region and marking it used. The graph itself lives in
a single `IrqLock<Graph>` static: the lock disables interrupts while held,
because the timer and the serial receiver will both mutate the graph from
interrupt context in phase 4.

Boot creates the root, `cpu0`, a `Device` for the serial console, and a
`MemoryObject` for every region of physical memory that is spoken for: the
kernel image, the framebuffer, the frame bitmap, and each boot module. Named
edges hang off the root for each. Then the in-kernel checker runs and the whole
graph is printed over serial.

`tools/graphdump.py` reads that block out of the serial log, re-verifies the
structural invariants from outside the kernel, and renders it with Graphviz.
`scripts/smoke.sh` now does this on every boot, so a disagreement between the
in-kernel checker and the offline one fails the build.

Measured at the milestone: 6 nodes, 10 edges, 123135 frames total with 117850
free, checker clean in kernel and on the host.

**What this phase caught.** `Graph::EMPTY` was not actually all zeroes:
`Process::ZERO` set `next_slot_hint` to 1, which put all 429 KiB of arenas into
`.data` and into the kernel image instead of `.bss`. The claim that the graph
costs nothing before the allocator runs was quietly false. Fixed by treating a
zero hint as "scan from the start", and `scripts/build-iso.sh` now fails the
build if `.data` exceeds 64 KiB so it cannot regress. Sections are now 288
bytes of `.data` against 465 KiB of `.bss`.

**Risk retired:** bootstrapping order. Nothing needed a frame before the bitmap
existed, and nothing needed a node before the graph was declared, because the
graph needs no construction at all.

## Running it

```
cargo ktest                 # graph crate tests, on the host
cargo kbench                # the fast-path measurements
./scripts/build-iso.sh      # build the kernel and a UEFI ISO
./scripts/qemu.sh           # boot it, serial on stdout
./scripts/smoke.sh          # boot headless, capture serial + screenshot + graph
./scripts/check.sh          # everything above that can fail a commit
```

`scripts/smoke.sh` writes `build/serial.log`, `build/screen.png` and
`build/graph.png`. There is no KVM in the usual container, so QEMU runs under
TCG.

### Phase 3: address spaces and the page-table invariant — **done**

Raw four-level page-table manipulation through the bootloader's direct map, and
`vm::map` / `vm::unmap` as the only functions in the kernel that write a user
page-table entry. Each creates or destroys the corresponding `Maps` edge in the
same operation, and rolls back both halves if either fails.

Invariant I5 is checked in **both** directions on every user address space:
every edge is backed by the entries it claims, and every entry that exists was
authorised by some edge. The second direction is the one that catches a leak of
authority rather than a loss of it, and it is the one a conventional kernel
structurally cannot check, because it has no central authority on what should
be mapped.

The milestone runs at every boot as `selftest::address_spaces`, and the build
fails if any leg misbehaves:

- map four pages, switch address spaces, write through the mapping, and read
  the bytes back from the physical frame;
- corrupt a leaf entry by hand: `checker caught it: 0x40000000 wants 0x6000,
  found 0x106000`;
- add a valid entry with no edge behind it: `checker caught it at 0x40200000`;
- unmap, map a different frame at the same address, and read: the new contents,
  proving the TLB flush is real;
- destroy everything and compare frame counts: 117778 before, 117778 after,
  page tables included.

**Deviation from the plan, stated deliberately.** The kernel keeps the
bootloader's page tables for its own half rather than building fresh ones, and
records them as two `Maps` edges on a `kernel-space` node. This is I5' from the
design: the kernel mapping is one fixed thing, immutable after boot, and exempt
from the checker's walk because verifying a direct map of all of RAM is O(RAM)
and proves nothing. Every user address space is created by Bramble and checked
in full. Building our own kernel tables is deferred until it buys something.

**Risk retired:** the derived-index consistency problem, which DESIGN 5.3 named
as the design's principal bug surface. It is not eliminated — no kernel can
prevent divergence from hardware it must write by hand — but it is now
detectable for the whole system by one routine, in both directions, in about
150 lines. That is the concrete payoff of having a single source of truth.

### Phase 4: threads, preemption, and the fast-path gate — **done**

Kernel threads with their own stacks, a context switch that saves the
callee-saved registers and the flags, the legacy PIC and PIT at 100 Hz, and a
timer handler that does three O(1) things and then considers switching.

The run queue is the `Ready` adjacency list of the `Cpu` node, exactly as
designed. Picking the next thread is `pick_and_rotate`: read the head, advance
it. No traversal anywhere on the path.

**The gate, and how it was nearly failed by a bad measurement.** The plan's
go/no-go is "the graph scheduler within 1.5x of a hand-rolled one". Measured as
the *decision in isolation*, the first result was 17.7x in debug and 12.1x in
release, which would have triggered DESIGN 5.3's fallback. That measurement was
wrong for the question: it isolates the one operation the graph is worst at and
excludes every cost the two designs share. What a kernel actually pays is a
whole context switch. So the phase now measures both and gates on the second,
with the counterfactual derived by subtracting a measured, separable component.

Final numbers, release build under TCG, best of five runs:

| Measurement | Value |
|---|---|
| Scheduler decision, graph run queue | 179 cycles |
| Scheduler decision, control intrusive list | 26 cycles |
| Decision ratio | 6.78x |
| Full context switch, measured | 4678 cycles |
| Full context switch, control (derived) | 4525 cycles |
| **Full switch ratio** | **1.03x** |
| The graph's share of a context switch | 3% |

**Verdict: go.** The graph really is several times slower at the decision, and
the decision is 3% of a switch.

**What halved the decision cost.** `rotate_ready` was calling the general
"move this edge to the tail" helper. For the *head* of a circular list that is
just advancing the head pointer, but the general path did an unlink and a
relink: about ten writes to reach a state one write describes. Fixing that, and
adding `pick_and_rotate` so the scheduler looks the cpu up once instead of three
times, took the decision from 349 to 179 cycles and the graph's share of a
switch from 7% to 3%. Neither change compromises anything; the first was a
missing special case and the second is the graph exposing the operation its
caller actually performs.

**What the checker did not catch.** The phase leaked exactly twelve frames: the
three thread stacks. The graph was entirely self-consistent the whole time —
every invariant held — because `spawn_kernel_thread` hung each stack off the
root rather than off its thread. The bookkeeping was coherent and wrong.

Fixed by adding `Owns: Thread -> MemoryObject` to the compatibility table. A
thread owning its own stack is what ownership is for: it means "dies with", and
a kernel stack dies with its thread. The randomised property tests now exercise
that edge, and there is a direct test that a reaped thread hands its stack back.

**Deviation from the plan.** The design calls for a LAPIC timer calibrated
against the PIT. v1 uses the PIT and the 8259 PIC directly: fifty lines instead
of three hundred, and on one core they do the same job. The PIC does not scale
past one core, so this becomes LAPIC work when SMP arrives (phase 9 item 4).

**Risk retired:** the fast-path cost, which was the whole reason this phase came
before userspace. It was worth finding out here that the honest answer is "the
graph costs several times more for the decision and it does not matter", rather
than in phase 8.

### Phase 5: userspace — **done**

Ring 3, `syscall`/`sysret`, an ELF64 loader, processes with handle tables, and
six system calls: `exit`, `yield`, `write`, `lookup`, `inspect`, `rights`. Two
user programs ship as boot modules, built from `user/`, a separate workspace
with its own linker script.

The milestone runs at every boot and fails the build if any leg misbehaves:

- `hello` prints through a capability carrying `Write`;
- the *same device* through a second capability without that right is refused
  with `E_PERM`, and an ungranted slot returns `E_BADHANDLE` — not "denied",
  but "there is nothing there to deny";
- a pointer into the kernel's half is refused with `E_FAULT`, checked against
  the caller's own `Maps` edges rather than a copy of them;
- `lookup("console")` mints a fresh capability, requiring a root capability
  carrying `Lookup`;
- `inspect` copies the entire kernel state into the program's buffer, which it
  decodes and counts by kind with no kernel help;
- `hello` exits cleanly and every frame comes back;
- `faulter` writes through a null pointer, is destroyed mid-instruction, and
  every frame comes back with all invariants still holding.

Measured: 117413 free frames before, 117413 after two processes lived and died.

**Six real bugs, all worth recording.**

1. **The boot context lost its own stack pointer.** Phase 4's teardown deleted
   the boot `Thread` node, so phase 5's first context switch had nothing to
   switch *back* to and discarded the outgoing stack pointer. Phase 5 re-adopts
   a boot thread.
2. **The system-call stub did not preserve caller-saved registers.** `dispatch`
   is an ordinary C function and treats `rdi`, `rsi`, `rdx`, `r8`-`r10` as
   scratch, but the caller's compiler assumes they survive. A program's own
   `write` destroyed the `self` pointer it was about to use. The ABI now says
   only `rcx` and `r11` are clobbered, and the stub honours it.
3. **A kernel stack overflow that failed silently.** `walk_out` returns a 2 KiB
   array *by value*, and the `lookup` path used three of them. Kernel stacks
   live in the direct map, so there is no guard page: the overflow walked into
   neighbouring physical memory and the machine simply stopped. Read-only walks
   now use the borrowing iterator, stacks are 8 pages, and every kernel stack
   carries a canary that the consistency pass checks.
4. **The kernel ran on an address space it had destroyed.** Killing a process
   switched to a kernel thread, which has no address space of its own, and
   `cr3` was left pointing at the dead process's tables. The reaper freed those
   frames, the allocator handed one straight back as the next process's page
   table root, and zeroing it wiped the live mappings. A thread with no address
   space of its own now runs in the kernel's.
5. **`swapgs` cannot be paired with Rust's `x86-interrupt` handlers.** Every
   entry from ring 3 must swap and every exit must undo it, but an
   `x86-interrupt` function generates its own prologue and cannot swap first, so
   a timer arriving during user code left the two bases crossed. v1 drops `gs`
   entirely and reads two absolute addresses instead. **Debt:** SMP needs a
   per-cpu block, which means hand-written interrupt stubs that swap
   conditionally on the saved code segment.
6. **A fault while holding the serial lock deadlocked the fault handler.** The
   crash paths now break the console locks first, the way Linux's
   `bust_spinlocks` does. Three of the bugs above presented as "the machine
   stops with no output"; this is why.

Also fixed: boot-module `MemoryObject`s recorded the bootloader's *virtual*
address as their physical base, and the ELF loader now lays images out per page
with the protections of every segment touching a page unioned, because lld
synthesises `.got` after any linker script has had its say and no script can
stop two segments sharing a page.

**Risk retired:** the ABI plumbing the plan expected to be the phase most
likely to eat a weekend on one bug. It ate several, and every one of them was
in the conventional machinery rather than in anything to do with the graph.
