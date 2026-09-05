# Bramble Dev Log

A running account of building the thing, written to be read by someone who has
never written a kernel. Every entry follows the same shape:

- **The concepts**, explained plainly, with no assumed background.
- **How it is normally done**, because you cannot appreciate a departure
  without knowing what it departs from.
- **What Bramble does instead**, and honestly whether it is actually different
  or just the same thing wearing a hat.
- **What broke**, because the bugs are where the design gets tested.

Entries are appended as phases land. The plan they follow is `PLAN.md`; the
reasoning behind the design is `DESIGN.md`. This file is the story.

---

## Entry 0 — What we are building, and why it is odd

### What a kernel actually does

When you turn a computer on, the hardware is a pile of parts that do not know
about programs, files, or users. It knows about memory addresses and
instructions. A **kernel** is the first program that takes charge and imposes
useful fictions on top of that:

- *"You are a process"* — a program that thinks it has the machine to itself.
- *"This is your memory"* — an illusion of private address space.
- *"This is a file"* — a name that gets you at some bytes.
- *"You may not touch that"* — permission.

To keep those fictions straight, a kernel has to *remember things*. Which
processes exist, what memory each one has, which of them is allowed to do
what, who is waiting for whom. The kernel's bookkeeping is the kernel.

### How that bookkeeping is normally arranged

Every mainstream kernel keeps it in several unrelated filing systems that were
each designed separately and do not talk to each other:

| The kernel needs to know | It uses |
|---|---|
| Which processes exist | A **process table** — a list |
| What memory a process has | A **page-table tree** — hardware-defined |
| What files exist | A **filesystem tree** — a hierarchy of directories |
| Who can do what | **Permission bits** and file descriptors, bolted on per subsystem |

Four different shapes, four different sets of rules, four different tools to
inspect them. On Linux, seeing all four means using `ps`, `/proc/PID/maps`,
`lsof`, and a debugger — and there is no way to *join* them, to ask one question
that spans all four. There is no single place that knows the whole truth.

### What Bramble does instead

Bramble keeps **one** structure: a typed directed graph.

If you have not met the term: a **graph** here is just *dots and arrows*. The
dots are called **nodes** and the arrows are called **edges**. "Typed" means
each dot and each arrow has a label saying what kind it is. That is the entire
idea. A social network is a graph: people are dots, "follows" is an arrow.

In Bramble:

- **Every kernel object is a dot.** A process is a dot. A thread is a dot. A
  chunk of memory is a dot. The serial port is a dot. An address space is a dot.
- **Every relationship is an arrow.** "This process owns that memory" is an
  arrow labelled `Owns`. "This process is allowed to write to the console" is an
  arrow labelled `Holds`. "This address space maps that memory here" is an arrow
  labelled `Maps`. "This thread is waiting on that channel" is an arrow labelled
  `Waiting`.

There is no process table, no filesystem tree, no separate permission system.
There is a graph, and those things are patterns *within* it.

### Why anyone would want this

Three things get much easier, and they are the reason to try:

1. **One picture of everything.** Because it is all one structure, one command
   draws the entire state of the kernel, joins included. We already have this
   working: see the picture in Entry 3.
2. **A kernel `fsck`.** Because there is one structure with stated rules, you
   can write a **checker** that verifies the whole kernel is internally
   consistent. Conventional kernels cannot do this across subsystem boundaries,
   because there is no "whole" to check.
3. **Questions become graph queries.** "Could this process ever reach that
   resource?" is a reachability question — a well-studied one, with a linear-time
   answer known since 1977. In a conventional kernel it is not a question you can
   ask at all.

And one thing gets harder, which we should say up front: **speed**. A graph
lookup is slower than following a pointer. Most of the engineering in this
project is about making sure that never lands on a path that runs millions of
times a second. More on that in Entry 2.

### Where the pushback went

The original brief wanted four things that sounded elegant and were, on
inspection, wrong. Each was replaced with a nearby version that keeps the
benefit. The full argument is in `DESIGN.md`, section 0; the short version:

- *"Permission is reachability"* → **permission is one arrow**. If A can reach
  C by going through B, transitive reachability would hand A everything B can
  see, which makes it impossible to confine anything. Reachability answers a
  different and still-useful question: what A could *ever* acquire.
- *"Scheduling is traversal"* → **the run queue is one node's arrow list**.
  Picking the next thread must be one memory read, not a search.
- *"Cleanup is reachability"* → **ownership is a tree**. Reclaiming a general
  graph by reachability is garbage collection, which inside a kernel is a
  research project. Constrained to a tree, "is it still owned?" is free.
- *"Open handles are nodes"* → **a handle is an arrow**. A dot would add a hop
  to every system call and model nothing extra.

---

## Entry 1 — Phase 0: getting a computer to run our code

**Milestone: the word `bramble` on screen, the memory map on the serial port,
and a deliberate crash that prints registers instead of rebooting.**

### The concepts

**Firmware and UEFI.** Before any operating system runs, code baked into the
motherboard runs first. On modern PCs that firmware is **UEFI**. It knows how to
read a disk and start one program. It does not know what an OS is.

**The bootloader.** Writing code that talks to UEFI directly is a project in
itself, so almost nobody does it. A **bootloader** sits in between: UEFI starts
the bootloader, the bootloader sets up a sane environment and starts your
kernel. We use one called **Limine**. It hands us four things we would otherwise
have to fight for: the CPU already in 64-bit mode, a map of physical memory, a
block of pixels to draw on, and all of physical memory conveniently visible at a
known address.

**Long mode.** x86 processors still boot pretending to be a 1978 chip. Getting
to modern 64-bit operation ("long mode") takes a specific dance of mode
switches. Limine does it for us. This is not cheating; it is the same reason you
do not write your own C compiler before writing a program.

**The framebuffer.** The simplest possible graphics: a rectangle of memory where
each 4 bytes is one pixel's colour. Write a number, a dot changes colour. No
driver, no GPU. To draw a letter you need the letter's shape as a bitmap — a
**font** — which we bake into the kernel from DejaVu Sans Mono with a script,
because the kernel has no font renderer and no memory allocator to run one in.

**The serial port.** A 1970s-era one-wire-at-a-time text channel. Every kernel
developer's best friend, because it needs almost no code to work, works before
graphics work, and QEMU can dump it straight into a file. This is where
Bramble's real output goes; the screen is a status display.

**GDT, TSS, IDT.** Three tables the x86 requires you to fill in.
- The **GDT** describes memory segments. Mostly a fossil in 64-bit mode, but you
  still have to provide one, and it is where the CPU learns the difference
  between "kernel privilege" and "user privilege".
- The **TSS** holds, among other things, a spare stack for handling a crash so
  bad that the normal stack is unusable.
- The **IDT** is the crash-and-interrupt dispatch table: 256 slots, each saying
  "if event number N happens, jump here". Event 14 is a page fault, event 6 is
  an invalid instruction, and so on.

Without an IDT, any mistake makes the CPU give up and reboot — a **triple
fault** — with no explanation. With one, you get a register dump. The difference
between those two is the difference between a debuggable project and a hobby you
abandon.

### How it is normally done

Exactly like this. There is nothing Bramble-specific about booting; every
hobby kernel and every real one does the same things in the same order.

### What Bramble does

The same. This phase exists to make the *later* phases possible, and the one
deliberate choice worth noting is that we spent effort on `scripts/smoke.sh`:
it boots the kernel headless, captures the serial log, takes a screenshot
through QEMU's monitor socket, and fails the build if an expected line never
appears. Every phase since has been able to prove itself automatically. That is
worth an afternoon.

### What broke

The `limine` Rust library we build against speaks version 9 of the Limine
protocol; the bootloader binaries we downloaded were version 8. Requests
renamed between those versions were silently ignored rather than rejected — the
kernel command line came back empty and everything else worked, which is the
most annoying kind of failure. Now pinned to matching versions, with the reason
written down in `limine.conf` so nobody wonders later.

---

## Entry 2 — Phase 1: the graph itself, and why it is hard in a kernel

**Milestone: the graph library, with 24 tests including randomised ones that
verify every invariant after every single operation, plus measurements.**

This is the phase that decides whether the whole idea is viable, which is why it
came second and not eighth.

### The concepts

**Why you cannot just use a normal data structure.** In ordinary programs you
write `Vec::new()` or `Box::new(...)` and memory appears. That works because a
**memory allocator** is running underneath. A kernel *is* the thing that provides
memory. At the moment Bramble starts, there is no allocator; and the allocator,
once it exists, will want to record what it has handed out — in the graph. So the
graph cannot depend on the allocator that depends on the graph.

Bramble's answer: **the graph never allocates**. It is a fixed set of arrays,
declared once, sized at compile time.

**Arenas.** An **arena** is a big fixed array of slots plus a note of which are
in use. Instead of "allocate a node", you say "take slot 47". Nothing to fail,
nothing to fragment, no allocator required.

**The dangling reference problem.** If a node lives in slot 47 and something
remembers "slot 47", and then that node is destroyed and slot 47 is reused for
something else, the remembering thing now silently points at a stranger. In C
this is *use-after-free*, and it is the source of a large fraction of the
security holes in the world.

**Generational indices** solve it. Each slot carries a counter. A reference is
not "slot 47" but "slot 47, generation 9". Destroying the occupant bumps the
counter to 10. Now the old reference fails a one-instruction check. It is a
version number on a locker: same locker, new lock, your old key does not fit.

Bramble adds a small trick: the counter's **odd or even** state encodes whether
the slot is occupied. So a single comparison answers both "is this the same
object?" and "is it still alive?" And because a fresh, never-used slot has
counter zero, **an array of all zeroes is a valid empty arena** — which is why
the whole graph can live in a region of memory the loader zeroes for free, with
no setup code at all.

**Linked lists, and the intrusive kind.** To find "all the arrows out of this
dot", you could search every arrow — far too slow. Instead each dot keeps the
first arrow of each kind, and each arrow keeps the next one. That is a **linked
list**.

Bramble's arrows are in *two* lists at once: the source dot's outgoing list and
the target dot's incoming list. This is an old sparse-matrix technique called an
**orthogonal list**, and it is what makes both of these one memory read:

- "What is this CPU running next?" — the first arrow in its `Ready` list.
- "Who holds a capability to this endpoint?" — its incoming `Holds` list.

That second one is the whole of **revocation**. Destroy a resource, walk its
incoming list, every permission to it is gone. seL4 needs a dedicated auxiliary
tree for this; Unix cannot do it at all — delete a file and processes that had it
open keep reading it.

The lists are **circular** (the last arrow points back to the first), which
means a single pointer gives you both ends, so adding to the back of a queue and
taking from the front are both instant. That is what makes the run queue a real
FIFO queue without extra bookkeeping.

**The checker.** A separate piece of code whose only job is to walk the whole
graph and verify every rule: every arrow's endpoints exist, every object has
exactly one owner, ownership never loops, every cached value equals what
following the arrow would give. It is the thing that makes the "one structure"
claim testable rather than aspirational.

### How it is normally done

Conventional kernels use raw pointers and reference counts:

- A relationship is a **pointer** — 8 bytes, one memory read. Very fast.
- Lifetime is a **reference count** — a tally of how many things point at an
  object, freed when it hits zero. Simple, but cycles leak, and every increment
  and decrement is a cost.
- Each subsystem invents its own lists and tables, with its own rules.

The result is fast and completely un-inspectable. There is no way to enumerate
"all relationships", because there is no such concept; there are only fields in
structs that happen to hold addresses.

### What Bramble does, and what it costs

Every relationship is an explicit 64-byte record with a type tag, attributes,
and its place in two lists. Compared to an 8-byte pointer, that is **8x the
memory**. For a machine with a few hundred kernel objects, this is irrelevant.
At production scale it would be disqualifying, and the honest thing is to say
so out loud.

In exchange: relationships can be enumerated, typed, attributed, checked, drawn,
and revoked uniformly. None of which a pointer can do.

**The type safety part.** Some rules are enforced by the *compiler*, not by
runtime checks. The rule "a `Ready` arrow goes from a CPU to a thread, never
anything else" is expressed so that writing the wrong thing does not compile. So
that rule costs zero instructions at runtime, because it cannot be broken.

### The measurements

Taken on the host, in release mode (`cargo kbench`):

| Operation | Time |
|---|---|
| Resolve a handle to an object, checking permission | 5.5 ns |
| Look up a node by id | 3.1 ns |
| Pick the next thread to run | 3.1 ns |
| Rotate the run queue (round robin) | 6.9 ns |
| Create and destroy one relationship | 49.9 ns |
| Check every invariant, 69 nodes / 261 arrows | 8.1 µs |

The number that actually matters is the last test: the fast path took **8.76 ns
on a tiny graph and 8.98 ns on a graph fifty times larger**. A ratio of 1.03.
The design lives or dies on that not growing, and it does not.

### What broke — and this is the interesting part

The randomised tests found three real bugs within seconds of existing. All three
were the *same kind* of bug, and it is the kind `DESIGN.md` predicted would
dominate this project: **a cached value drifting away from the arrow it was
supposed to shadow.**

Some things are too slow to look up through the graph on a hot path, so the
answer is copied into the object — a "hot-hop cache". The graph is still the
truth; the copy is a convenience. But now there are two places holding one fact,
and they can disagree.

1. **A stale address-space pointer.** Nothing stopped a thread from having *two*
   address spaces. Adding a second one updated the cached pointer but the graph
   still reported the first. Fixed by making that relationship single-valued —
   setting a new one replaces the old — and by adding checks that count arrows
   and complain if a "one of these" relationship has two.

2. **A thread blocked forever.** If a thread was waiting on a communication
   channel and the channel was destroyed, the waiting arrow was removed but the
   thread was left marked "blocked" with nothing to wake it. That is a deadlock,
   and it is exactly the failure the plan expected to find in *phase 7*, six
   phases later. Fixed with explicit abort semantics: the cleanup process now
   reports the stranded thread by name so the kernel can wake it with an error.

3. **A half-finished state change.** Blocking a thread means removing it from
   the run queue and attaching it to what it waits on. If the second step failed
   — because the target was already being destroyed — the first had already
   happened, leaving a thread marked "runnable" that nothing would ever run.
   Fixed by validating everything *before* touching anything.

The lesson worth carrying: **the graph itself was never the problem.** Not one
bug was in the arena, the generational indices, or the list splicing. Every bug
was in a cache. And the checker found all three in seconds, which is the entire
argument for having built it first.

---

## Entry 3 — Phase 2: physical memory, and the first real graph in the kernel

**Milestone: the kernel boots, prints its entire state as a graph, and a host
tool re-verifies and draws it.**

### The concepts

**Frames.** Physical memory is managed in fixed 4096-byte chunks called
**frames** (or physical pages). Everything is a whole number of frames.

**The memory map.** Not all addresses are memory. Some are hardware pretending
to be memory, some are firmware's, some are broken. The firmware provides a list
saying which ranges are genuinely usable. It is messier than you would expect —
on our QEMU machine, 31 entries with usable RAM in pieces between reserved
regions.

**A frame allocator** hands out free frames and takes them back. Bramble uses
the simplest possible design: **a bitmap**, one bit per frame, 1 for used. For
462 MiB of RAM that is a 16 KiB table.

**The bootstrap problem.** The bitmap itself needs memory, and it is the thing
that hands out memory. Bramble carves it out of the first usable region by hand
and immediately marks those frames used. One special case, at the very
beginning, and then everything else is uniform.

**`.bss` versus `.data`.** A program's variables that start as *zero* go in a
section called `.bss`, which is not stored in the file at all — the loader just
zeroes that much memory. Variables with a non-zero starting value go in `.data`,
and every byte is stored in the file. That distinction matters here more than
usual, and it bit us. See below.

### How it is normally done

Linux uses a **buddy allocator** (splitting and merging power-of-two blocks) and
keeps a `struct page` for every single frame — a per-frame record, millions of
them, several dozen bytes each. It is genuinely a large fraction of the kernel's
memory.

### What Bramble does

Two decisions, and the split between them is the honest heart of the design.

**The bitmap is deliberately *not* in the graph.** `DESIGN.md` section 4.4 keeps
a list called the non-graph register: everything the kernel holds that is not a
node or an arrow, each with a written reason. The reason here is that *a set of
free frames is not a relationship*. Nothing has a relationship with a free
frame; that is what free means. Modelling one node per frame would make the graph
a million dots that say nothing and cost 64x what a bitmap costs.

**But every frame that is spoken for is described by a node.** The moment a
frame is handed out and outlives the call, a `MemoryObject` node describes it.
So at boot, Bramble creates a node for the kernel's own code, one for the
framebuffer, one for the bitmap's own storage, one for each boot module, and one
covering all of physical memory. Each gets an `Owns` arrow from the root and a
`Named` arrow giving it a human-readable name.

This is the compromise stated plainly: **free memory is anonymous and fast; used
memory is accountable.**

**Names as arrows.** A `Named` arrow carries a string. `lookup("console")` walks
the root's `Named` arrows. That is the whole naming system. Notice what falls
out for free: two names for one object is just two arrows (`console` and `tty0`
both pointing at the serial port — there is a test for it). In Unix that needs
symlinks or hard links, each with their own rules. Here it is not a feature; it
is the absence of a restriction.

### One structure, two checkers

The kernel prints its whole state over the serial port between two markers.
`tools/graphdump.py` reads that back on the host and does two things: renders it
with Graphviz, and **re-verifies the structural rules independently**. The
in-kernel checker and the host checker examine the same claims from opposite
sides, and `scripts/smoke.sh` fails the build if they ever disagree.

This is the "one picture of everything" benefit arriving early and cheaply. The
picture in `build/graph.png` is the actual state of the running kernel.

### What broke

The claim that the graph "costs nothing in the kernel image because it is all
zeroes" was **quietly false**. One field — a hint about where to start looking
for a free permission slot — started at 1 instead of 0. One non-zero byte
anywhere in the structure means the compiler cannot use the all-zeroes
shortcut, so all **429 KiB** of arenas moved out of `.bss` and into the kernel
image itself.

Nothing misbehaved. It just silently made the kernel 429 KiB larger and added a
startup copy, falsifying a stated design property. Fixed by treating a zero hint
as "start from the beginning", and `scripts/build-iso.sh` now **fails the build**
if `.data` exceeds 64 KiB, so it cannot come back. Sections are now 288 bytes of
`.data` against 465 KiB of `.bss`.

The general lesson: a design property nobody measures is a design property you
do not have.

---

## Entry 4 — Phase 3: virtual memory, and the first real test of the thesis

**Milestone: map memory, prove the mapping works, then deliberately corrupt it
two different ways and prove the checker catches both.**

This is the most important phase so far, because it is where the central claim
meets hardware that does not care about our opinions.

### The concepts

**Virtual memory.** Every program gets its own private set of addresses. When a
program reads address `0x40000000`, that is not a real location in RAM; the CPU
translates it, via tables the kernel writes, into some actual frame. Two programs
can both use `0x40000000` and get different memory. This is how isolation works,
and it is enforced by silicon, not by trust.

**Page tables.** The translation tables. On x86-64 they are a four-level tree.
To translate one address the CPU walks four levels, each a 4096-byte table of
512 entries, using nine bits of the address at each level. Each final entry holds
a frame address plus permission bits: may write, may execute, may user code touch
this. The format is dictated by the hardware, down to the bit.

**The MMU** is the part of the CPU that does this walk, in hardware, on every
single memory access.

**The TLB.** Walking four levels for every access would be ruinously slow, so
the CPU caches recent translations in the **Translation Lookaside Buffer**. This
creates a trap that has bitten every kernel ever written: if you change a page
table, the CPU may keep using the *old* cached translation. You must explicitly
tell it to forget. Miss that, and a program keeps reading memory you already gave
to someone else — the kind of bug that becomes a security advisory.

**Page faults.** When a program touches an address with no valid translation, the
CPU stops and calls the kernel. That is a page fault. It is how a kernel
implements demand loading, copy-on-write, swap, and process termination.

### How it is normally done, and the thing nobody says out loud

Real kernels keep **two** descriptions of the same fact.

1. A high-level list of what *should* be mapped. Linux calls these VMAs; each
   says "addresses X to Y come from this file or this anonymous memory, with
   these permissions". This is what the kernel reasons with.
2. The **actual page tables** the hardware reads.

These two must agree. Nothing checks that they agree. They are maintained by
different code, in different files, and a mismatch does not announce itself — it
shows up later as corruption, or as a program reading memory it should not.

This is a real and famous source of bugs. It exists because there is no shared
notion of truth: the VMA list is one subsystem's opinion and the page tables are
another's.

### What Bramble does

`DESIGN.md` calls it **invariant I5**, and it is stated as a promise:

> **Page tables are a cache of `Maps` edges.**

Meaning: the arrow in the graph is the *truth*. The page tables are a derived
copy that exists only because the hardware insists on reading its own format. It
is the same two-descriptions situation every kernel has — with two differences
that change everything:

1. **There is exactly one place that writes a user page-table entry.** The
   functions `vm::map` and `vm::unmap`, which create or destroy the arrow in the
   *same operation*. Not "should be kept in sync"; cannot be separated.
2. **There is a checker that verifies it, in both directions.** Not a test on
   some paths — a routine that walks all of it.

Both directions matter, and they catch opposite failures:

- **Graph → hardware:** for every `Maps` arrow, every page it covers has an
  entry pointing at the right frame with matching permissions. Catches a
  mapping the kernel *thinks* exists but does not — a program crashing on memory
  it was promised.
- **Hardware → graph:** walk every entry that actually exists and confirm some
  arrow authorised it. Catches a mapping that exists but was never granted —
  **that is the security-relevant direction**, a leak of access nobody
  authorised.

Almost no kernel checks the second direction, because doing so requires a
central authority on what *should* be mapped. Bramble has one because the graph
is that authority by construction.

### The four experiments

The kernel runs these at every boot, and the build fails if any of them does not
behave. From the real serial log:

**1. A mapping exists in both places, or in neither.** Create an address space,
allocate four pages, map them, run the checker. Then switch the CPU to that
address space, write a pattern through the virtual address, switch back, and read
the *physical* frame directly to confirm the bytes arrived. Not "the tables look
right" — the hardware actually did the translation.

**2. Corrupt a page-table entry by hand.** Reach in and rewrite one entry to
point at the wrong frame — precisely the divergence that goes undetected in a
conventional kernel:

```
i5:   corrupted a pte by hand; checker caught it: 0x40000000 wants 0x6000, found 0x106000
```

Caught, with the address, the frame it should have been, and the frame it was.

**3. Add a mapping with no arrow behind it.** Write a valid page-table entry
directly, granting real access to real memory that the graph never authorised.
This is what a privilege-escalation bug looks like from the inside:

```
i5:   added a pte with no edge; checker caught it at 0x40200000
```

Caught. This is the direction that a conventional kernel structurally cannot
check.

**4. Prove the TLB is really flushed.** Rather than hoping, we make it
observable: unmap the pages, allocate a *different* set of frames with different
contents, map them at the *same* virtual address, and read. A stale cached
translation would show the old contents. It shows the new ones.

Then everything is destroyed and the frame count is compared with the count from
before the test — 117778 before, 117778 after. No leak, including the page tables
themselves, which the cleanup process returns because the graph tells it that an
address space was freed and hands back the tables' location.

### What this actually bought

Something worth being precise about. Bramble does not *prevent* the page tables
from diverging from the graph — no kernel can, since the hardware format has to
be written by hand eventually. What it does is make divergence **detectable, by
one routine, for the whole system, in both directions.**

That is a smaller claim than "impossible" and a much larger one than what a
conventional kernel offers, which is nothing. And it cost about 150 lines,
because the graph already knew what was supposed to be true. In a kernel with no
central authority on that question, this checker could not be written at all.

### An honest note on the kernel's own page tables

The plan said this phase would build the kernel's own page tables from scratch,
replacing the bootloader's. It does not. It adopts the bootloader's tables for
the kernel half and records them as two `Maps` arrows.

This is deliberate and stated in `DESIGN.md` as invariant I5': the kernel's own
mapping is one fixed thing, set once at boot and never changed, and it is exempt
from the checker's walk because verifying a direct map of all of RAM is O(RAM)
and proves nothing. The teeth of I5 are on *user* address spaces, which are
created, changed and destroyed constantly, and those are checked completely.
Building our own kernel tables is deferred to whenever it buys something.

---

## What is next

Phase 4 is threads and preemption, and it carries the second go/no-go gate: a
context switch that picks the next thread out of the graph must come within
1.5x of a hand-written list-based scheduler. If it does not, there is a
pre-agreed fallback that gives up graph storage for that one relationship while
keeping it visible in the picture.

It is also the first phase where the kernel does more than one thing at a time,
which means the locking discipline written down in Entry 2 stops being theory.
