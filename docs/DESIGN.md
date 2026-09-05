# Bramble: Design Document (v0.1, pre-implementation)

Status: draft for review. No code exists yet. Everything here is a proposal
and the sections marked **Pushback** are places where I think the original
brief is wrong or under-specified and should be changed before coding starts.

Companion document: `docs/PLAN.md` (the phased implementation plan).

---

## 0. Summary of pushback

The core idea, one typed graph as the kernel's only bookkeeping structure, is
sound as a *data model*. Four of the consequences in the brief, taken
literally, are not sound as *fast-path mechanisms*. Each has a nearby
formulation that keeps the benefit and drops the cost.

| Brief says | Problem | Proposed reformulation |
|---|---|---|
| "Permissions are reachability" | Transitive reachability is exactly the wrong authority rule: if A can reach C via B, A gets C's authority without B's cooperation, and confinement becomes impossible. | **Authority is one hop** (a `Holds` edge with rights). **Potential authority** (what A could ever obtain) is reachability, and that is the take-grant safety query from 1977. Both are graph queries; only the second is transitive, and it never runs on a fast path. |
| "Scheduling is traversal over the runnable subgraph" | Picking the next thread by traversing anything is O(runnable). A run queue is a graph too; the thing that makes it fast is that it is an *ordered adjacency list* with O(1) head/tail ops. | The run queue is the `Ready` adjacency list of a `Cpu` node. Pick-next is "first out-edge". Scheduling *policy* can use graph structure (blocked-on chains, direct switch to the IPC partner); pick-next never traverses. |
| "Cleanup is reachability-based rather than refcount-based" | Reachability-based reclamation of a general graph is tracing garbage collection. A tracing GC inside a kernel with no heap and bounded-latency interrupt handlers is the single riskiest thing in the brief, and Phantom OS spent years on exactly that. | Constrain the liveness relation to a **tree** (`Owns` edges, rooted at `Root`). Reachability over a tree *is* "has an owner", which is O(1) to maintain and needs no counting and no tracing. Destroying a node destroys its subtree and revokes every capability to anything in it. A tracing pass exists only as a *checker*. |
| "Open handles are nodes" | A node per handle doubles node count and adds a pointer hop to every syscall for no modelling benefit. | A handle **is an edge**: `Process --Holds{rights, slot}--> Object`. A handle becomes a node only if it needs private state (a file offset, say). v1 has none. |

Two further concerns that are not contradictions but should be decided
consciously:

- **"Everything is inspectable as a graph" survives fully. "The fast path
  executes graph queries" does not survive at all.** Every hot operation in
  this design reads an O(1) index that is *derived from* the graph and kept
  consistent with it by the edge operations. Section 5 lists every such index.
  If you want a kernel whose page-fault handler literally walks edges, this
  document is not that kernel and I do not think that kernel can exist on x86.
- **Persistence is the unaddressed elephant.** With no filesystem and no
  storage, v1's graph is the whole world and evaporates at power-off. That is
  fine for v1. But "no filesystem hierarchy" will eventually mean "a persistent
  graph", and a persistent graph with crash consistency is a database engine.
  KeyKOS and EROS solved it with whole-system checkpointing and it dominated
  their engineering. Decide now that v1 through v3 do not touch it.

---

## 1. Assumptions and the questions I would have asked

This session ran unattended, so instead of blocking on questions I made the
following calls. Each is reversible at the design stage; please correct any
that are wrong.

| # | Question | Assumed answer | Why it matters |
|---|---|---|---|
| Q1 | Synchronous rendezvous IPC (seL4-style, sender blocks until receiver arrives) or asynchronous queued messages? | **Synchronous, with a small register-passed payload and at most one capability per message.** | Async queues need message nodes (node churn per message) and put capabilities "in transit" inside kernel objects, which is the one place the ownership tree can hide a cycle. Synchronous IPC has neither problem and is what the fast-path numbers in section 5 assume. Async notifications can be added in a later phase as a bitmask on the `Endpoint` node. |
| Q2 | Does a dead owner revoke, or does an object live while anyone holds it? | **Owner death revokes** (KeyKOS space-bank / Zircon job semantics). | This is the ownership-tree decision from section 0. The alternative is GC. |
| Q3 | One thread per process in v1? | **Yes**, but `Thread` and `Process` are separate node types from day one so multi-threading is an edge-count change, not a schema change. |
| Q4 | Is there any persistent storage in v1? | **No.** Userspace programs are static ELF64 binaries loaded by Limine as boot modules. No disk, no filesystem, no block layer. |
| Q5 | Eager or lazy mapping? | **Eager in v1.** A user page fault is always a fault (kill the process). Lazy mapping arrives with the range index in a later phase. |
| Q6 | Is an in-kernel query language wanted? | **No.** The kernel exports a snapshot; queries run in userspace over the snapshot. The kernel offers exactly two fixed queries: lookup-by-name and inspect. |
| Q7 | Which "purity" compromises are acceptable? | The list in section 4.4. I want you to bless that list explicitly; it is the contract that keeps the thesis honest. |
| Q8 | Benchmarks in QEMU: TCG or KVM? | **KVM where available** (`-enable-kvm`). Under TCG, cycle counts are meaningless in absolute terms and only useful as ratios. The plan's go/no-go numbers assume KVM. |
| Q9 | Userspace language and runtime? | Rust `no_std`, no libc, a tiny `bramble-user` crate wrapping the syscalls. |
| Q10 | Fixed-capacity arenas acceptable for v1? | **Yes.** Exhaustion returns an error to the caller, never panics. Growth by chunking is designed in (section 3.6) but not built in v1. |

---

## 2. The thesis, restated so that it can be true

Bramble's kernel state is a single typed, directed, attributed graph
**G = (N, E)**. Every kernel object is a node with a type and a fixed-size
body. Every relationship is an edge with a type and a fixed-size attribute
block. The graph is the **source of truth**: any other data structure in the
kernel is either (a) an index derived from G and kept consistent by the
operations that mutate G, or (b) explicitly listed in the non-graph register
(section 4.4).

From this follow the four properties the brief wants, in their corrected
forms:

1. **Authority is an edge.** A process may perform operation *op* on object *x*
   iff there is an edge `Process --Holds{rights ∋ op}--> x`. No transitivity.
2. **Scheduling state is edges.** A thread is runnable iff `Cpu --Ready--> Thread`
   exists; it is blocked iff `Thread --Waiting--> (Endpoint | Device)` exists.
3. **Lifetime is a tree.** Every node except `Root` has exactly one incoming
   `Owns` edge. A node is live iff it is reachable from `Root` along `Owns`
   edges, and because `Owns` forms a tree this is equivalent to "its `Owns`
   in-edge exists".
4. **Names are edges.** `Root --Named{"console"}--> Device`. A path is a walk
   along `Named` edges. Nothing in the kernel privileges that walk.

The **uniform inspect syscall** returns G. The **uniform checker** validates G
against the hardware (page tables) and against the indices. Those two, not the
fast path, are where the graph pays for itself.

---

## 3. The graph data structure

### 3.1 Requirements, in priority order

1. **No dynamic allocation to exist.** The graph must be usable before any
   allocator runs, because the allocator's own state (the frames it has handed
   out) is described by the graph.
2. **Bounded-latency mutation.** Every operation callable with interrupts
   disabled must be O(1) or O(small constant). Unbounded work (cascading
   deletion, snapshots) must be deferrable to thread context.
3. **Stable, forgeable-safe identifiers.** IDs held by userspace (via handle
   slots) and by other nodes must detect use-after-free.
4. **O(1) typed adjacency.** "First `Ready` out-edge of this `Cpu`" and "first
   `Waiting{Recv}` in-edge of this `Endpoint`" must be one load.
5. **Attribute storage on edges.** `Maps` needs a range and protection, `Holds`
   needs rights and a slot, `Named` needs a name.
6. **Locking that does not foreclose SMP.**
7. **Cheap to snapshot** into a userspace-readable form.

### 3.2 Candidate A: typed slab arenas + generational IDs + intrusive orthogonal edge lists

**Node storage.** One statically sized slab per node type, in `.bss`:

```
static GRAPH: SpinLock<Graph>;

struct Graph {
    threads:     Slab<Thread,      256>,
    processes:   Slab<Process,     128>,
    spaces:      Slab<AddressSpace,128>,
    memobjs:     Slab<MemoryObject,1024>,
    endpoints:   Slab<Endpoint,    512>,
    devices:     Slab<Device,      16>,
    cpus:        Slab<Cpu,         1>,     // grows with SMP
    root:        Slot<Root>,
    edges:       Slab<Edge,        16384>,
    // free-list heads per slab, mutation counter for snapshot seqlock
}
```

Each `Slot<T>` is `{ gen: u32, state: u8, body: MaybeUninit<T> }` plus a
fixed **node header** shared by all types:

```
struct NodeHeader {
    gen:      u32,
    kind:     NodeKind,        // u8
    flags:    u8,              // DYING, PINNED, ...
    out_head: [EdgeIdx; N_EDGE_KINDS],   // 7 × u32
    in_head:  [EdgeIdx; N_EDGE_KINDS],   // 7 × u32
    owner:    NodeId,          // hot-hop cache of the Owns in-edge (see 5.3)
}
```

Per-type heads cost 56 bytes per node and buy requirement 4 exactly: the
first edge of a given kind in a given direction is one array load.

**Identifiers.**

```
NodeId = { kind: u8, idx: u24, gen: u32 }   // 8 bytes, Copy
EdgeId = { idx: u32, gen: u32 }             // 8 bytes, Copy
```

Encoding the kind in the ID means lookup goes straight to the right slab
with no dispatch table. Generation 0 is reserved as "never valid", so
**all-zero memory is an empty, valid graph**. That is why `.bss` works with no
initialisation loop, and why a zeroed handle slot is a null handle.

**Edge storage.** A single edge slab. Each edge is a cross-linked list node in
two lists at once: its source's out-list for its kind, and its target's
in-list for its kind ("orthogonal list", the sparse-matrix trick):

```
struct Edge {                     // 64 bytes, one cache line
    src: NodeId,                  // 8
    dst: NodeId,                  // 8
    kind: EdgeKind, flags: u8,    // 2
    _pad: u16,                    // 2
    gen: u32,                     // 4
    out_next, out_prev: EdgeIdx,  // 8
    in_next,  in_prev:  EdgeIdx,  // 8
    data: EdgeData,               // 24, a union keyed by kind
}
```

`EdgeData` payloads for v1 (all fit in 24 bytes):

| Kind | Payload |
|---|---|
| `Owns` | none |
| `Holds` | `rights: u32`, `slot: u32` |
| `InSpace` | none |
| `Maps` | `vaddr: u64`, `len_pages: u32`, `off_pages: u32`, `prot: u8` |
| `Ready` | `prio: u8` (unused in v1's round-robin) |
| `Waiting` | `role: Send/Recv`, `badge: u64` |
| `Named` | `name: [u8; 23]` + `len: u8` (longer names are a post-v1 string table) |

**Operations and their cost.**

| Op | Work | Bound |
|---|---|---|
| `lookup(NodeId)` | slab index + generation compare | O(1), 1 load |
| `lookup(EdgeId)` | same | O(1) |
| `first_out(node, kind)` / `first_in(node, kind)` | 1 load | O(1) |
| `link(src, kind, dst, data)` | pop edge free-list, push onto 2 lists (head or tail) | O(1), ~6 stores |
| `unlink(edge)` | splice out of 2 lists, push to free-list, bump gen | O(1), ~6 stores |
| `move_to_tail(edge)` | unlink + link in the same list | O(1) |
| `retarget_src(edge, new_src)` | splice out of one out-list, into another | O(1); this is capability transfer |
| `delete_node(n)` | unlink every incident edge | O(degree), see 3.7 |
| `snapshot()` | copy all live nodes and edges | O(N + E), slow path only |

**ID stability across deletion.** A freed slot's generation is incremented
before it goes on the free list; the slot is reused only with the new
generation, so every outstanding `NodeId`/`EdgeId` to the old occupant fails
its compare. At 32 bits, wraparound needs 4 billion reuses of *one slot*; the
`u32` is retained for compactness, and a slot whose generation would wrap is
retired instead of reused (a cheap, complete fix).

**Cost of this candidate.** Roughly 1.5 MB of `.bss` at the sizes above
(edges dominate: 16384 × 64 B = 1 MB). Two dependent loads per handle
dereference (edge, then node). 64 bytes per relationship where a
conventional kernel spends 8 (a pointer) or 0 (an array index). Per-type list
heads make every node header 72+ bytes before its body.

### 3.3 Candidate B: relationships as struct fields (the conventional design, dressed up)

Each node type is a Rust struct whose relationships are fields:
`Thread { space: NodeId, run_link: ListLink, wait_link: ListLink, ... }`,
`Process { handles: [Holds; 256], children: ListHead, ... }`. Edges are not
stored anywhere; the inspect syscall *synthesises* them by reading the
fields.

This is fastest (it is what Linux and seL4 do) and is the honest fallback if
candidate A proves too slow. It is rejected as the primary design because it
gives up the thesis entirely: there is no uniform edge, no uniform attribute,
no uniform revocation ("find every reference to x" is a per-type special
case, which is precisely why seL4 needed the CDT), and adding a relationship
means editing structs. The inspect view would be a reconstruction, and the
checker would have nothing to check against.

Its one idea survives in candidate A as the **hot-hop cache** (section 5.3):
a handful of fields that shadow specific unique edges.

### 3.4 Candidate C: sorted edge table (CSR / triple store)

All edges in one array sorted by `(src, kind, dst)`, with a per-node offset
table. Queries are binary searches and range scans; the representation is
compact (16 to 24 bytes per edge), cache-friendly, and trivially snapshotted.

Rejected for the live graph: insertion is O(E) shifting or requires a B-tree,
which requires allocation and has worst-case latencies that are wrong for
interrupt context. **Adopted as the snapshot wire format** (section 5.4): the
inspect syscall emits exactly this, sorted, so userspace tools get a CSR for
free and never have to chase intrusive links.

### 3.5 Other candidates considered briefly

- **Hash maps keyed by `(src, kind)`.** Non-deterministic latency, needs
  resizing, needs allocation. No.
- **Per-page nodes** (a node for every physical frame, a `Maps` edge per PTE).
  4 GB of RAM is a million nodes and the graph *becomes* the page table, only
  64× larger. No. `MemoryObject` describes ranges; the page table is a cache of
  `Maps` range edges (section 4.3, invariant I5).
- **Bitmaps as edges** (free frames as edges from a "free pool" node). A set is
  not a relationship. Free memory stays a bitmap (section 4.4).

### 3.6 Choice: candidate A, with C as the export format

Candidate A is chosen. What it costs, plainly:

- **Memory:** ~64 B/edge, ~100 to 400 B/node. A process with 1000 capabilities
  costs 64 KB of edges. Irrelevant for a hobby kernel; disqualifying for a
  production one.
- **Latency:** every reference is ID → slab → generation check, roughly two to
  five nanoseconds and one potential cache miss more than a raw pointer.
  There are two to four of these on each syscall.
- **Fixed capacity in v1.** Post-v1 growth: each slab becomes an array of
  chunk pointers (static) whose chunks are frames from the frame allocator;
  `idx → (idx >> k, idx & mask)`. Nothing moves, so IDs stay stable. The
  `Slab` API is written from day one so that swapping in chunked storage
  touches no caller.
- **Type erasure of edge attributes.** `EdgeData` is a union. Rust type safety
  is recovered at the API boundary with a trait per edge kind:

  ```
  trait EdgeKind { const KIND: u8; type Src: NodeKind; type Dst: NodeKind; type Data; }
  impl EdgeKind for Maps { type Src = AddressSpace; type Dst = MemoryObject; type Data = MapsData; }
  fn link<K: EdgeKind>(g: &mut Graph, src: Ref<K::Src>, dst: Ref<K::Dst>, data: K::Data) -> EdgeId
  ```

  Endpoint-type compatibility (invariant I3) then holds at **compile time** for
  every call site, with a runtime assertion only on the one generic path used
  by the checker and the snapshot decoder.

### 3.7 Allocation before the allocator, and the boot sequence

The circularity in the brief ("no cycles in the allocator") dissolves because
**the graph never allocates**. There is no kernel heap in v1 at all: no
`alloc` crate, no `Box`, no `Vec`. There are exactly two sources of memory:

1. The static slabs in `.bss`, which exist the moment the kernel is loaded.
2. A physical **frame allocator** (bitmap over usable regions) that is a
   conventional data structure, *not* part of the graph, and whose only
   customers are kernel stacks, page-table pages, `MemoryObject` backing, and
   (post-v1) slab growth chunks.

Boot order, showing what exists at each step:

| Step | Action | Memory it uses |
|---|---|---|
| 1 | Limine enters `_start` in long mode with paging on, hands over HHDM offset, memory map, framebuffer, modules. | none |
| 2 | Static GDT, IDT, TSS; serial port for logging. | `.bss` |
| 3 | Frame allocator: bitmap over usable regions. The bitmap's own storage is carved from the first usable region large enough and marked used. | one region's head |
| 4 | Graph is already valid (zeroed `.bss`). Create `Root` and `Cpu0`. Create Root-owned `MemoryObject`s for the kernel image, the framebuffer, the bitmap, and each boot module. Create the kernel `AddressSpace` node. | slabs |
| 5 | Build the kernel's own page tables (frames from step 3), map HHDM and kernel image, load CR3. Record them as two `Maps` edges on the kernel `AddressSpace` (see I5's exemption for the kernel half). | frames |
| 6 | LAPIC timer (calibrated once against the PIT), then threads, then userspace. | frames for stacks |

Everything the frame allocator hands out that outlives the call is described
by a `MemoryObject` node. Free frames are not in the graph: nothing has a
relationship with a free frame.

### 3.8 Locking, interrupts, and the SMP door

**v1: one global graph lock**, a spinlock that saves and disables interrupts.
seL4 runs its SMP configuration on a big kernel lock and is fine to eight
cores; a hobby kernel does not need to do better in v1.

Rules that hold from day one so SMP is not foreclosed:

1. **No raw pointers into the graph escape a lock guard.** Everything outside
   the guard holds `NodeId`/`EdgeId` and re-validates. This is what lets a
   later design replace the global lock with per-node locks, a seqlock, or an
   RCU-like scheme without touching callers.
2. **Fast-path reads never take the graph lock.** Handle lookup reads the
   per-process slot table and the edge/node it points at; page walks are done
   by hardware. These are the derived indices of section 5.3, and they are
   published with release semantics by the mutation that creates them.
3. **Every critical section is bounded.** The maximum lock hold time is the
   maximum single graph op. The two unbounded operations are handled thus:
   - **Node deletion** is two-phase. Phase 1 (under lock, O(1)): set `DYING`,
     unlink the `Owns` in-edge, unlink any `Ready`/`Waiting` edge. From this
     instant the node is unreachable from `Root`, the scheduler will never
     touch it, and every handle dereference fails the `DYING` check. Phase 2
     (the **reaper** kernel thread, lock taken and released per edge or per
     child): unlink remaining edges, recurse into owned children, free the
     slot, return frames.
   - **Snapshot** copies under the lock in v1 (a few thousand nodes is tens of
     microseconds) and moves to a seqlock-retry scheme when that is too long.
4. **Interrupt handlers do O(1) or nothing.** Timer: set `need_resched`,
   acknowledge, return. Serial RX: push a byte into a static ring buffer, set
   a flag. The graph mutation that a wakeup implies (`Waiting → Ready`, three
   list splices) runs on the interrupt-return path under the lock, which is
   permitted because it is O(1).
5. **Context switch releases the lock on the far side.** The scheduler picks
   under the lock, switches stacks, and the incoming thread's first action
   is to release it (the `finish_task_switch` pattern). The lock is never held
   across anything that can block.
6. **Per-CPU state is a node.** Run queues are `Ready` adjacency of a `Cpu`
   node. Adding a core is adding a node; migrating a thread is
   `retarget_src` on one edge. When SMP arrives, `Cpu` nodes get their own
   locks and the global lock retreats to structural mutations.

---

## 4. The type system

### 4.1 Node types (v1: eight)

| Kind | Body (fixed size) | Notes |
|---|---|---|
| `Root` | tick counter, boot info summary, arena occupancy | Exactly one. The only node with no `Owns` in-edge. |
| `Cpu` | `current: NodeId`, `need_resched`, kernel stack pointer, per-CPU scratch | One in v1. `current` is in the non-graph register (4.4). |
| `Process` | handle table `[EdgeIdx; 256]`, next-free-slot hint | The capability container. Equivalent to seL4's CSpace root plus Zircon's process. |
| `Thread` | saved callee-saved registers, kernel stack top, user `rsp`, trap-frame pointer, `state`, IPC message buffer (8 words + 1 slot), cached `cr3`, cached owner `NodeId` | One per process in v1. |
| `AddressSpace` | PML4 physical address, mapping count | The page-table root is *owned by* this node, not described by other nodes. |
| `MemoryObject` | physical base, page count, flags (device memory, kernel-pinned), refcount of `Maps` in-edges (cache) | Describes a contiguous physical range. Non-contiguous objects are post-v1 (a page list in a chunk). |
| `Endpoint` | nothing beyond the header in v1 | All its state is `Waiting` in-edges. Post-v1: notification bitmask. |
| `Device` | device class, port/MMIO base, IRQ line, RX ring pointer | v1: the serial console only. The framebuffer is a `MemoryObject` with the device flag, not a `Device`. |

The whole system state for the v1 goal is roughly: 1 `Root`, 1 `Cpu`,
1 `Device`, 3 `Process` (init + two workers), 3 `Thread`, 4 `AddressSpace`
(kernel + 3), ~20 `MemoryObject`, 1 or 2 `Endpoint`. Fewer than fifty nodes.
The arenas are sized 20× to 50× larger so exhaustion tests are possible.

### 4.2 Edge types (v1: seven) and the compatibility table

| Kind | Src → Dst | Meaning | Cardinality |
|---|---|---|---|
| `Owns` | `Root → {Cpu, Device, Process, AddressSpace, MemoryObject, Endpoint, Thread}`; `Process → {Process, Thread, AddressSpace, MemoryObject, Endpoint}` | Storage and lifetime. Parentage is `Process Owns Process`. | Exactly one in-edge per non-root node. |
| `Holds` | `Process → {Process, Thread, AddressSpace, MemoryObject, Endpoint, Device}` | Capability. `rights` is a bitmask; `slot` is the userspace handle number. | Any. |
| `InSpace` | `Thread → AddressSpace` | Which page tables to run under. | Exactly one out-edge per thread. |
| `Maps` | `AddressSpace → MemoryObject` | A virtual range backed by a physical range. | Any; ranges within one space must not overlap (I7). |
| `Ready` | `Cpu → Thread` | Runnable, queued on this CPU. List order is queue order. | At most one in-edge per thread. |
| `Waiting` | `Thread → {Endpoint, Device}` | Blocked in send or receive. List order is FIFO wait order. | At most one out-edge per thread. |
| `Named` | `Root → any` | A human-readable binding. v1 has one namespace. | Any. |

`Ready` and `Waiting` are mutually exclusive on a thread, and a thread with
neither is either `Running` (it is some `Cpu`'s `current`) or `Dying`.

Things deliberately **not** edge types in v1, and what they are instead:

- **Parentage**: it is `Owns`. Decoupling (reparenting orphans to init) is a
  one-edge-kind addition later.
- **IPC channel**: it is two `Holds` edges to the same `Endpoint`, and while a
  rendezvous is pending, a `Waiting` edge. There is no persistent
  "channel" edge because synchronous IPC has no persistent channel state.
- **Handles**: `Holds` edges (section 0).
- **Running**: a field on `Cpu`, exported as a virtual edge by inspect.

### 4.3 Invariants and where each is enforced

| # | Invariant | Enforced at | Cost of enforcement |
|---|---|---|---|
| I1 | Both endpoints of every edge are live nodes with matching generations. | **Structural.** Edges live in both endpoints' lists; deleting a node unlinks all incident edges before the slot is freed. Cannot be violated by a well-typed caller. Checker re-verifies. | zero on fast path |
| I2 | `Owns` is a tree rooted at `Root`: every non-root node has exactly one `Owns` in-edge; no cycles. | **Construction.** `create_node(owner, ...)` is the only constructor and it links the `Owns` edge atomically. `reparent` (post-v1) checks that the new owner is not a descendant: O(depth), off the fast path. | zero |
| I3 | Edge kind is compatible with `(src.kind, dst.kind)` per the table in 4.2. | **Compile time** via the `EdgeKind` trait for every typed call site; runtime assertion on the generic path. | zero |
| I4 | A `DYING` node is unreachable from `Root`, has no `Ready`/`Waiting` edge, and no handle dereference succeeds on it. | **Deletion phase 1** does all three under one lock hold. Handle lookup checks the flag. | one byte load per syscall |
| I5 | **Page tables are a cache of `Maps` edges.** For every `Maps` edge in a user `AddressSpace`, every page of the range has a present PTE to the right frame with protection ⊆ the edge's `prot`; and every present user PTE belongs to exactly one `Maps` edge. | **Construction.** The only two functions that write user PTEs are `link::<Maps>` and `unlink::<Maps>`, which update tables and flush TLB before returning. **Checker** walks every user address space's tables and diffs against edges. | PTE writes on map/unmap; zero elsewhere |
| I5' | The kernel half of every address space is one fixed mapping (HHDM + image) recorded as two `Maps` edges on the kernel `AddressSpace`, and is immutable after boot. | **Construction.** Exempt from the checker's PTE walk (verifying a 1:1 mapping of all RAM is O(RAM) and tests nothing). | zero |
| I6 | `thread.state == Ready` ⇔ exactly one `Ready` in-edge; `== Blocked` ⇔ exactly one `Waiting` out-edge; `== Running` ⇔ some `Cpu.current == thread`. | **Construction.** One function, `set_state(thread, new)`, performs the edge change and the field write under the lock. Checker verifies. | zero |
| I7 | `Maps` ranges within one address space do not overlap. | **Construction**, O(mappings) scan in v1 (mappings per space are single digits). Post-v1: the per-space range index makes this O(log n). | map-time only |
| I8 | `Process.handles[slot]` is either zero or the index of a live `Holds` edge whose `src` is that process and whose `data.slot == slot`. | **Construction** by `grant`/`revoke`. Checker verifies. | zero |
| I9 | Hot-hop caches (`Thread.cr3`, `Thread.owner`, `NodeHeader.owner`, `MemoryObject.map_count`) equal what following the corresponding edge would yield. | **Construction**: the edge ops for `InSpace`, `Owns`, `Maps` write the cache. Checker verifies. | a store per edge op |
| I10 | Every `Waiting{Send}` thread's message buffer is valid and every `Waiting{Recv}` thread has a free slot reserved if it accepts capabilities. | **Construction** in `send`/`recv` entry. | zero |

Enforcement philosophy: **construction time wherever the API can be shaped
to make violation unrepresentable; the checker for everything else; debug
assertions only as a tripwire on the checker's own assumptions.** The checker
is not optional tooling. It is the mechanism by which the derived indices
(I5, I8, I9) are kept honest, and it must exist before the first index does
(see the plan: the checker ships in phase 1, the first index in phase 3).

The checker runs: (a) after every operation in host-side property tests;
(b) in-kernel on a debug syscall; (c) in debug builds, every N timer ticks
from the reaper thread. A checker failure in the kernel is a panic with a
graph dump, not a log line.

### 4.4 The non-graph register (the principled exceptions)

Everything the kernel holds that is **not** in G, with the reason. This list
is a contract: adding to it needs a design note, and the inspect syscall
reports each entry as an attribute so the "entire kernel state" claim holds.

| State | Where | Why not in the graph | Exported as |
|---|---|---|---|
| Free-frame bitmap | frame allocator | A set, not a relationship. Would be a million edges. | `Root` attribute: free/used counts |
| Page tables | frames owned by an `AddressSpace` | Hardware-dictated format; a cache of `Maps` (I5). Also the only place the hardware writes (A/D bits) that the graph does not know about. | not exported; checker cross-verifies |
| `Cpu.current` | `Cpu` body | Written on every context switch; an edge would add two list splices to the switch for no query benefit. | virtual `Running` edge |
| Handle tables | `Process` body | Index: slot → `Holds` edge. The syscall fast path's first load. | reconstructible from `Holds.slot` |
| Hot-hop caches | node bodies | Shadow one unique edge each (I9). | none needed |
| Slab free lists, generations | `Graph` | The graph's own plumbing. | `Root` attribute: occupancy |
| Serial RX ring buffer | `Device` body | Bytes are not objects. | none |
| Kernel stacks | frames | Owned via a `MemoryObject` per thread; the *contents* are not modelled. | n/a |
| Timer tick count | `Root` body | Scalar. | `Root` attribute |

Post-v1 additions already anticipated: a per-`AddressSpace` **range index**
(sorted array or radix over `Maps` edges keyed by `vaddr`) once lazy mapping
exists, and a **string table** for names longer than 23 bytes.

---

## 5. The fast path

### 5.1 Where a literal reading of the thesis is fatal

Each of the following is an O(n) or worse traversal that a literal
"everything is a graph query" design would put on a hot path. Each is
rejected, with the O(1) replacement.

| Literal design | Cost | Replacement |
|---|---|---|
| Permission check as multi-hop reachability from process to object | O(reachable subgraph) per syscall | One-hop `Holds` edge, found through the slot table. |
| Pick-next as traversal of the runnable subgraph from `Root` | O(N) per switch | Head of the `Cpu`'s `Ready` list. |
| Page fault resolved by walking `Maps` edges | O(mappings) per fault, in the worst possible context | Hardware page tables (I5). Eager mapping in v1 means a user fault is always fatal and never consults the graph. |
| Handle lookup by scanning the process's `Holds` out-list for a slot | O(handles) per syscall | `Process.handles[slot]` table (I8). |
| Wakeup by scanning all threads for those waiting on this endpoint | O(threads) per IRQ or send | The endpoint's `Waiting` in-list, head only. |
| Object liveness by tracing from `Root` | O(N + E) per delete | `Owns` tree; delete = unlink one edge (I2, I4). |
| Frame allocation by finding an unowned frame node | O(frames) per allocation | Bitmap, outside the graph. |
| Address-space lookup on context switch by scanning thread out-edges | O(out-degree), small but a list walk on the hottest path | `Thread.cr3` hot-hop cache (I9). |

### 5.2 Per-operation cost of the chosen design

Counts are dependent memory loads on the graph (each a potential cache miss)
and list splices (each ~3 stores). Cycle estimates are for KVM on a modern
core and are targets for the plan's go/no-go gates, not promises.

| Operation | Graph loads | Splices | Other dominant cost | Target |
|---|---|---|---|---|
| Handle dereference | 3 (`handles[slot]` → edge → node) + flag/gen checks | 0 | none | < 20 cycles over a raw pointer |
| Timer preemption | 2 (`current` thread, head of `Ready`) | 2 (`current` → tail, head → out) | register save, CR3 write, TLB refill | switch ≤ 1.5× a hand-rolled list scheduler |
| `send` with receiver already waiting | 3 (endpoint via handle) + 1 (head of `Waiting`) | 1 (unlink receiver's `Waiting`) | 8-word copy, direct switch | round trip < 2000 cycles under KVM |
| `send` with no receiver | same | 1 (link sender `Waiting`) + scheduler | context switch | |
| Capability transfer inside `send` | +1 (sender's `Holds` edge) | 1 (`retarget_src` or new link) + slot allocation | none | |
| `map` | 2 | 1 | PTE writes per page, TLB flush | not a fast path |
| Process exit | O(owned subtree) | O(edges in subtree) | frame return | reaper, off the critical path |
| `inspect` | O(N + E) | 0 | copy to user | slow path, ~100 µs at v1 sizes |

The two numbers that decide whether the thesis holds are the **context
switch overhead ratio** and the **IPC round trip**. The plan measures the
first in phase 4 and the second in phase 6, and measures the raw list ops on
the host in phase 1, which is early enough to predict both.

### 5.3 The compromises, named

Where purity is traded for speed, in the order I expect them to matter:

1. **Derived indices** (slot tables, page tables, hot-hop caches). The graph
   is truth; these are caches maintained by the edge ops and audited by the
   checker. This is the compromise that makes the design workable, and it is
   also the design's principal bug surface. Every kernel bug class I can
   predict is "a cache diverged from an edge". That is why the checker is
   phase 1 and why I would run it on every tick in debug builds.
2. **`Cpu.current` is a field, not an edge.** Two splices per switch saved.
3. **Authority is one-hop.** Not a performance compromise; a correctness one,
   but it is the largest departure from the brief's wording.
4. **Deletion is two-phase.** Unreachability is instantaneous; storage return
   is deferred. A snapshot taken between the phases shows `DYING` nodes with
   dangling `Holds` in-edges. The inspect format flags them.
5. **The kernel half of the address space is exempt from I5's checker walk.**
6. **Free memory is not in the graph.**
7. **Range queries are not native.** "Which mapping covers address v?" needs a
   side index once lazy mapping exists.

Everything else in the brief survives intact.

### 5.4 The inspect syscall and the snapshot format

`inspect(buf, len) → needed_len | error`. The kernel writes, in this order:

1. A header: format version, node count, edge count, mutation counter,
   non-graph register values.
2. A node table: `(NodeId, kind, flags, body-summary)` with a fixed 64-byte
   summary per kind (enough to render; not the raw body).
3. An edge table **sorted by `(src, kind, dst)`** with attributes, plus a
   per-node offset table. This is candidate C, so userspace has a CSR.
4. Virtual edges (`Running`) appended and flagged as such.

Snapshot consistency in v1: taken under the graph lock. The mutation counter
is included so a userspace tool can detect that two snapshots straddle a
change. Post-v1: seqlock retry with the lock dropped between chunks.

Userspace tooling (host-side, Python or Rust, not part of the kernel):
render to Graphviz DOT, diff two snapshots, run the checker's invariants
against a snapshot, and answer the take-grant "could A ever obtain X" query.
None of this belongs in the kernel.

---

## 6. What this buys, and what gets worse

### 6.1 Capabilities that fall out naturally

- **One debugger for everything.** A single snapshot shows every process,
  mapping, capability, wait queue, and name, with the same tool. In a
  conventional kernel this is five tools (`ps`, `/proc/*/maps`, `lsof`, wait
  channel inspection, `mount`) that cannot be joined.
- **A kernel `fsck`.** One checker validates the entire kernel state against
  the hardware. Conventional kernels have per-subsystem debug options that
  cannot see across subsystem boundaries. For a hobby OS this is the feature
  most likely to save weeks.
- **Revocation is free.** Deleting a node unlinks its `Holds` in-list.
  seL4 needs the capability derivation tree for this; Mach needs no-senders
  notifications; Unix cannot do it at all (a deleted file stays open).
- **Confinement analysis is a query.** "Can process A ever obtain access to
  X?" is take-grant reachability over `Holds` edges plus endpoint `Grant`
  rights, decidable in linear time, runnable in userspace on a snapshot.
- **Deadlock detection is a query.** `Waiting` edges plus `Holds` edges *are*
  the wait-for graph. Cycle detection needs no extra bookkeeping.
- **Priority inheritance and direct switch need no extra structure.** "Who is
  the running thread about to hand off to?" is one hop.
- **Resource accounting is a subtree sum.** Charging a process for everything
  it and its descendants own is a walk of the `Owns` subtree. cgroups exist
  to bolt this onto a kernel that lacks it.
- **Names are cheap and plural.** Aliases, per-process views, renaming, and
  unnamed objects are all just edge operations. Plan 9 needed mount tables;
  Unix needs symlinks, bind mounts, and namespaces to approximate it.
- **New resource types are cheap.** A node kind, its compatibility rows, and
  its body. Lifetime, authority, naming, inspection, and checking are
  inherited.
- **SMP migration is one edge move.**

### 6.2 What gets worse

- **Memory per relationship: 8× to 64×.** Irrelevant here, disqualifying at
  scale.
- **Two extra dependent loads on every syscall.** Measurable, probably under
  10 percent of a minimal syscall, and the plan gates on it.
- **Coarse locking.** One graph means one lock in v1, and the "everything is
  one structure" thesis is in direct tension with "everything is
  per-CPU", which is how real kernels scale. Section 3.8 keeps the door open;
  walking through it will be hard.
- **The cache-coherence bug surface.** Every derived index is a place where
  truth and reality can diverge, and page tables are a derived index. A
  conventional kernel has the same bugs but does not *promise* consistency,
  so it does not have to detect them.
- **Range queries are foreign.** Anything keyed by address needs a side
  structure; the graph gives nothing for free here.
- **No POSIX, ever.** No filesystem hierarchy and capability-only authority
  means no existing software runs without a compatibility layer that would
  itself be a large project. This is a hobby OS; say so and move on.
- **Persistence is unsolved** (section 0).
- **Fixed capacities and a static footprint.** 1.5 MB of `.bss` before the
  first process, and hard limits until chunked growth is built.
- **Type erasure in `EdgeData`.** Recovered by the trait API but paid for in
  wasted padding and one more place to get wrong.
- **Two-phase deletion is visible.** Snapshots can show half-dead nodes.

---

## 7. Prior art

Nothing I know of stores a general typed edge set as the kernel's *own*
source of truth with the fast paths reading indices derived from it. Every
component of the design, however, has been built before, and in several
cases the builders' compromises predict this design's.

**Take-grant (Lipton & Snyder, 1977; Jones, Lipton, Snyder).** The formal
graph model of capability systems. Subjects and objects are nodes, rights are
labelled edges, and the four rules (take, grant, create, remove) rewrite the
graph. Its main theorem is that the safety question "can *p* ever obtain
right *r* to *q*" is decidable in time linear in the graph. This is the
"permissions are reachability" idea, done correctly: reachability answers
*potential* authority, one hop answers *current* authority. Read it first.

**Hydra (CMU, 1974) and CAP (Cambridge, 1970s).** The first object-capability
kernels. Hydra's local name spaces per procedure are `Holds` edges. Both
stopped at research scale; both had the transitive-authority problem in view.

**KeyKOS (Key Logic, 1980s).** The closest ancestor. The system is a graph of
*nodes* (each with exactly 16 *key* slots) and *pages*; a key is a capability,
so the whole system is literally a fixed-fan-out directed graph. Memory is
accounted through *space banks*, a hierarchy of storage owners, which is the
`Owns` tree. Persistence was whole-system checkpointing. Where it stopped:
commercially; and the fixed 16-slot node shape was a straitjacket that
Bramble's variable-degree edge lists avoid at the cost of a slab.

**EROS (Shapiro et al., 1990s to 2005) and CapROS / Coyotos.** KeyKOS
reimplemented, with a proof of confinement that is explicitly a reachability
argument over the capability graph, and a fast IPC path. Persistence again
dominated the engineering; Coyotos, the successor that dropped some of it,
stalled around 2009. Lesson: the persistence layer is a separate project.

**seL4 (NICTA/UNSW, 2009 to present).** Capabilities live in CNodes (a trie,
which is the slot table generalised); page tables are first-class objects;
*untyped memory* retyping forms an ownership tree that gives revocation and
accounting, which is `Owns`; and the *capability derivation tree* used for
revoke is stored as an intrusive doubly-linked list threaded through the
capability slots, which is exactly candidate A's `Holds` in-list. seL4 has
no unified graph and no naming at all (userspace's problem), runs SMP on a
big kernel lock, and its integrity and authority-confinement proofs are
statements about reachability over the capability graph. Bramble's I-list
is a subset of seL4's invariants with the page-table one (I5) being the
direct analogue of seL4's "VSpace consistent with cap state".

**Mach (CMU, 1980s).** Ports are capabilities; port rights move by message;
cleanup is refcount-based with "no senders" notifications. Its message-queued
IPC is the async design rejected in Q1, and its port-right accounting is the
tangle that synchronous IPC avoids.

**Plan 9 (Bell Labs, 1990s).** "A path is one query among many" in
embryo: namespaces are per-process mount tables, so the same name can mean
different things to different processes. It kept the hierarchy, and made
every resource a file server, which is the opposite direction from Bramble
(uniform *interface* rather than uniform *state*).

**Fuchsia / Zircon (Google, 2016 to present).** Kernel objects, handles with
rights, no filesystem in the kernel, names in userspace, and a **job tree**:
jobs own processes, and killing a job kills its subtree. That is the `Owns`
tree with owner-death-revokes semantics, shipping in production. Each object
kind still has its own data structures; there is no unified inspection.

**Barrelfish (ETH, 2009 to ~2020).** Two relevant pieces. The *system
knowledge base* is a Prolog (ECLiPSe) database in userspace holding hardware
and resource facts, queried for policy decisions, never on the fast path:
this is the "kernel state as a database" thesis with the same
off-the-fast-path compromise proposed here. And its capability *mapping
database* (MDB), used for revoke and range queries over capabilities, was
implemented as a balanced tree after a list proved too slow, which is the
range-index warning in 5.3.

**Genode (2008 to present).** A strict parent-child component tree where
children are funded by parents' resource quotas and die with them. `Owns`
tree again, with accounting as a subtree sum.

**Phantom OS (Zavalishin, 2000s to present).** Orthogonal persistence of a
whole object graph with garbage collection as the reclamation mechanism.
Its experience is the data point behind rejecting "cleanup is reachability":
Phantom ended up with two collectors, a fast refcounting one and a slow,
rarely run tracing one, because tracing the live graph was unaffordable as
the primary mechanism. Lisp machines and Smalltalk systems are the same story.

**Windows NT object manager.** Typed kernel objects with a namespace of
object directories, handle tables with per-handle access masks, refcounted
lifetime. The type system and handle model resemble Bramble's; the
namespace is a tree and lifetime is refcounting.

**DBOS (Stonebraker, Zaharia et al., 2020s).** Operating-system state held in
a distributed DBMS and manipulated by queries. The inspectability thesis at
cluster scale, built above Linux rather than replacing the kernel's own
structures. Confirms the market for the idea and the fast-path compromise.

**Theseus (Boos et al., OSDI 2020).** A Rust OS that uses the language's
ownership system, rather than a data structure, as the resource-tracking
mechanism, and organises the kernel around avoiding "state spill". Worth
reading as the alternative answer to the same itch: Theseus says the graph
should live in the type system; Bramble says it should live in memory where
it can be inspected at runtime.

**Verdict.** You are not reinventing a finished thing. You are combining the
KeyKOS/seL4 capability graph, the Zircon/Genode ownership tree, Plan 9's
edge-like naming, and the Barrelfish/DBOS "state as a queryable database"
idea into one explicit structure that the kernel itself uses as truth. The
parts are all proven. The combination is the risk, and the specific risk is
the derived-index consistency problem in 5.3, which none of the ancestors
had to solve because none of them promised a single source of truth.
Read Lipton & Snyder, the KeyKOS architecture paper, the EROS confinement
paper, and the seL4 reference manual's chapters on CSpace and untyped
retyping before phase 1.

---

## Appendix A: v1 syscall surface

Ten syscalls. Everything takes handles (slot numbers), never `NodeId`s.

| Syscall | Semantics |
|---|---|
| `exit(code)` | Phase-1 delete of the calling process; never returns. |
| `yield()` | Move `current` to the tail of `Ready`. |
| `write(dev, buf, len)` | Requires `Holds{Write}` on a `Device`. v1: serial only. |
| `read(dev, buf, len)` | Blocks with `Waiting` on the `Device` until RX ring non-empty. |
| `endpoint_create() → slot` | New `Endpoint` owned by and held by the caller with all rights. |
| `send(ep, words[8], cap_slot?)` | Synchronous. Requires `Send`; `Grant` if a capability is attached. |
| `recv(ep, &words, &cap_slot) → badge` | Synchronous. Requires `Recv`. |
| `call(ep, ...)` | `send` then `recv` on the reply, with direct switch. |
| `grant(target_proc, slot, rights_mask) → slot'` | Copy a capability into another process with reduced rights. Requires `Holds{Grant}` on the target process. Used by init to set up children. |
| `spawn(elf_memobj, ...) → proc_slot` | Create `Process` + `Thread` + `AddressSpace` owned by the caller, map the ELF segments and a stack, leave it un-`Ready` until `start`. |
| `start(proc)` | Link the `Ready` edge. |
| `mem_create(pages) → slot`, `map(space, memobj, vaddr, prot)` | Needed by `spawn`'s userspace half; may stay kernel-internal in v1. |
| `inspect(buf, len) → needed` | Section 5.4. |
| `lookup(name) → slot` | The one name query: copies `Root`'s `Named` target into the caller's table if the caller holds `Root` with `Lookup` rights. Only init does. |

Rights bits (u32): `Read`, `Write`, `Map`, `Send`, `Recv`, `Grant`, `Lookup`,
`Manage` (delete, reparent, start).

## Appendix B: sizes and limits (v1)

| Item | Value |
|---|---|
| `NodeId`, `EdgeId` | 8 bytes each |
| `Edge` | 64 bytes |
| `NodeHeader` | 72 bytes (with 7 edge kinds) |
| `Thread` body | ~320 bytes |
| `Process` body | ~1.1 KB (256-slot handle table) |
| Static graph footprint | ~1.5 MB `.bss` |
| Kernel stack per thread | 16 KB, from the frame allocator |
| Handle slots per process | 256 |
| IPC payload | 8 × u64 + 1 capability |
| Name length | 23 bytes inline |
