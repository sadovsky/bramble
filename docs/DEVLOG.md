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

---

## Entry 5 — Phase 4: doing more than one thing at once, and the moment of truth

**Milestone: three threads sharing one core, and a measurement that decides
whether the whole idea is viable.**

This is the phase the plan was ordered around. Everything so far could have been
thrown away cheaply. From here on, if the design is too slow, we find out having
built five phases on it.

### The concepts

**A thread** is one flow of execution: a place in the code plus a stack. A
single processor core can only run one at a time, so the illusion of several is
made by switching between them fast enough that nobody notices.

**A stack** is scratch memory a running function uses for its local variables
and for remembering where to return to. Every thread needs its own, because that
memory *is* the thread's position in its work.

**A context switch** is the act of swapping one thread for another:

1. Save the outgoing thread's registers onto its stack.
2. Write down where its stack pointer ended up.
3. Point the stack pointer at the incoming thread's stack.
4. Restore its registers from there.
5. Return — and because the stack changed, you return into different code.

It is genuinely eerie the first time: one function is entered by one thread and
left by another. Bramble's is 17 instructions.

**Preemption.** Two ways a thread stops running. It can **yield** — politely
hand over. Or it can be **preempted** — interrupted mid-instruction by a timer
and switched away without consenting. Preemption is what stops one buggy program
from freezing the machine, and it is the difference between cooperative
multitasking (Windows 3.1, classic Mac OS) and every serious system since.

**The timer.** A chip that raises an interrupt at a fixed rate — here 100 times
a second. The handler does three things and no more: count the tick, tell the
interrupt controller it is handled, then possibly switch. Doing anything slow in
there stalls the entire machine.

**Why the flags register matters.** A subtle one. Whether interrupts are enabled
lives in a register called RFLAGS. If a thread yields with interrupts off and we
switch to a thread that was suspended with them on, and the switch does not
carry RFLAGS across, the second thread resumes with interrupts disabled — and is
never preempted again. Bramble's switch saves and restores the flags, so this
cannot happen. A newly created thread gets a hand-built stack with the flags
value already set to "interrupts on", which is a pleasing way to make a thread
start correctly by construction rather than by remembering.

### How it is normally done

A run queue: a linked list of threads. Take the front one, run it, put it at the
back. Every kernel has one, usually several with priorities. Linux's has been
rewritten roughly every decade.

The list is **intrusive** — the "next" and "previous" pointers live *inside* the
thread structure rather than in separate list cells. So queueing a thread
allocates nothing, and pick-next is one memory read.

### What Bramble does

The same thing, described differently. The run queue **is** the `Ready` arrow
list of the CPU node. Picking the next thread is reading the head of that list.

The design was explicit that this had to be true: `DESIGN.md` pushed back on the
original idea that "scheduling is traversal over the runnable subgraph", because
searching for the next thread would be fatal. A run queue is already a graph —
what makes it fast is that it is an *ordered adjacency list* with instant access
to both ends. Bramble keeps the graph and keeps that property.

### The moment of truth, and a measurement that was asking the wrong question

The plan set a gate: **the graph scheduler must be within 1.5x of a hand-rolled
one**, with a pre-agreed fallback if not. So the kernel contains a control — a
plain intrusive list, the conventional implementation — and measures both.

The first result:

```
graph run queue        526 cycles/op
control list            27 cycles/op
ratio             19.26x
```

Nineteen times slower. The kernel panicked on its own gate, exactly as designed.

But that measurement was answering the wrong question. It timed the *decision
alone*: the one operation where the graph is at its worst, with every cost the
two designs share excluded. No kernel pays for a scheduling decision in
isolation. It pays for a whole context switch — saving registers, swapping
stacks, restoring — and the decision is a small part of that.

So the phase now measures both, and gates on the second:

| Measurement | Value |
|---|---|
| Decision, graph | 179 cycles |
| Decision, control list | 26 cycles |
| Decision ratio | 6.78x |
| **Full context switch, measured** | **4678 cycles** |
| Full context switch with a control queue (derived) | 4525 cycles |
| **Full switch ratio** | **1.03x** |
| The graph's share of a context switch | **3%** |

The counterfactual is a subtraction rather than a second implementation, and
that is worth naming: the decision is a measured, separable, serial part of the
switch, so removing its excess cost is legitimate arithmetic — but it is derived,
not observed, and the log should say so.

**The honest summary: the graph is genuinely about seven times slower at
choosing the next thread, and it does not matter, because choosing is 3% of
switching.** That is the shape the design predicted, and it is the shape that
makes the whole project viable. If the number had been 30%, the fallback would
have applied.

### The part where measuring made the code better

Nineteen times was suspicious even for a graph. Looking properly, the round-robin
step was calling a general helper meaning "take this arrow out of the list and
put it at the back". For the *front* of a **circular** list, that is just moving
the head pointer along one — the arrow is already in the right place. The general
path was doing about ten memory writes to reach a state one write describes.

Fixing that, plus adding a combined operation so the scheduler looks the CPU up
once instead of three times, took the decision from 349 to 179 cycles and the
graph's share of a switch from 7% to 3%.

Neither change gives anything up. The first was a missing special case. The
second is a small principle worth stating: **a data structure should expose the
operation its caller actually performs**, not make the caller assemble one out of
primitives and pay for the seams.

### The bug the checker could not catch

Twelve frames leaked — 48 KiB. Exactly three thread stacks.

The checker found nothing, and was right not to. Every invariant held. Every
arrow's endpoints existed, ownership was a proper tree, every cache matched.
**The bookkeeping was perfectly coherent and still wrong**, because
`spawn_kernel_thread` hung each stack off the root rather than off the thread
that used it. Nothing was inconsistent. Something was merely untrue.

This is worth dwelling on, because it marks the limit of what a checker buys
you. Invariants verify that the structure says what it means to say. They cannot
verify that it says the right thing. A stack owned by the root is a perfectly
legal graph; it just describes a world where stacks outlive their threads, which
is not the world we are in.

What caught it was the *other* kind of test: count the free frames before,
count them after, demand they match. Cheap, dumb, and it found what the clever
machinery could not.

The fix was to make the graph able to say the true thing. `Owns` now permits a
thread to own memory, and a thread's stack hangs off the thread. Because
ownership means "dies with", the reaper hands the frames back automatically — no
special case, no cleanup code. The right shape made the behaviour fall out.

That is the argument for the ownership tree in miniature: it is not that
reclamation is clever, it is that once lifetimes are stated correctly there is
nothing left to get wrong.

### What preemption actually looked like

Two worker threads counting in tight loops, never yielding, plus the boot thread
waiting. If preemption did not work, the first worker would get the CPU and hold
it forever.

```
sched: after 45 ticks, worker a did 857973 rounds and worker b did 859683
```

Within 0.2% of each other, and neither ever asked to be interrupted. That is the
timer taking the CPU away 100 times a second and the graph deciding who gets it
next.

### A deviation worth recording

The design calls for the modern per-core timer (the local APIC), calibrated
against the old one. Bramble uses the 1981 chips — the 8259 interrupt controller
and the 8253 timer — directly. Fifty lines instead of three hundred, and on a
single core they do the same job.

This is a genuine debt, not a shortcut without cost: the 8259 does not scale past
one core, so it must be replaced when SMP arrives. It is written down in the plan
as such. The general rule being followed: take the simpler thing when it is
equivalent *today*, and write down what it will cost tomorrow.

---

## What is next

Phase 5 is userspace: the first code that runs without permission to do whatever
it likes. That means the CPU's privilege levels, the system call instruction, and
the first real use of the capability system — a program that can print only
because it holds an arrow saying it may, and that fails cleanly when the arrow is
taken away.

---

## Entry 6 — Phase 5: the first code that is not trusted

**Milestone: a program that can print only because it holds an edge saying it
may, and a program that misbehaves and is destroyed without taking anything
with it.**

This is where the capability system stops being a diagram.

### The concepts

**Privilege rings.** The processor has four privilege levels; everyone uses two.
**Ring 0** may do anything: change page tables, talk to hardware, halt the
machine. **Ring 3** may not. The kernel runs in ring 0, programs in ring 3.
This is enforced by silicon, not by good manners.

**System calls.** A ring 3 program that wants something done asks the kernel.
The `syscall` instruction jumps to a fixed address in ring 0. It is a doorway
with exactly one entrance, and everything on the other side is the kernel's
choice.

The awkward part: on arrival, the stack pointer still points at the *user's*
stack, which the kernel cannot trust. It has to find its own stack before doing
anything, using only registers. Real kernels stash a per-core pointer in the
`gs` segment base and use a special instruction, `swapgs`, to flip between the
user's `gs` and the kernel's. More on that below, because it bit us.

**ELF.** The file format executables come in. Its useful part is a list of
**segments**: "take these bytes from the file, put them at this address, with
these permissions". A loader maps memory and copies bytes in. That is all
Bramble's loader does — no dynamic linking, no relocations, no interpreter.

**Handles.** Userspace never sees a kernel pointer or an internal id. It gets
small integers — **slot numbers** — that index a per-process table. Slot 1 might
mean the console. This is `open()` returning `3` in Unix, and it is the same
idea everywhere.

### How authority is normally decided

In Unix, mostly by **who you are**. Every process has a user id. When you open
a file, the kernel compares your id against the file's permission bits. Your
authority is *ambient*: it applies to everything you do, whether you wanted it
to or not.

That has a well-known consequence. A PDF viewer you run inherits your ability to
read every file you own, because it is running as you. It does not need that
authority, and cannot easily give it up. The mismatch between "what a program
needs" and "what it is handed" is where a great deal of security goes wrong, and
it is called the **confused deputy** problem: a program with authority is
tricked into using it on someone else's behalf.

The alternative, **capabilities**, is older than Unix and keeps losing on
adoption rather than on merit. Authority is not a property of who you are but a
thing you *hold*. You can only act on what you were handed. There is no ambient
anything.

### What Bramble does

Authority is a `Holds` edge, and there is no other kind.

When the kernel creates the `hello` process it grants exactly three
capabilities, and that is the entire universe the program can affect:

```
Holds Process -> Device  rights=RW  slot=1     the console, writable
Holds Process -> Device  rights=R   slot=2     the same console, not writable
Holds Process -> Root    rights=l   slot=3     the root, lookup only
```

Slots 1 and 2 point at *the same device*. The program is identical in both
calls. Only the edge differs:

```
[user] slot 1 (console) carries RW, slot 2 (same console) carries R, slot 3 (root) carries l
[user] writing through the read-only capability was refused, as it should be
```

There is no user id anywhere in Bramble, and no plan to add one. The check is
one edge lookup: find slot 1 in the handle table, follow it to the `Holds` edge,
test the rights bits, follow it to the object. Three dependent loads.

Then the part that is not merely "denied":

```
[user] an ungranted slot names nothing, so there is nothing to refuse
```

Writing through slot 200 does not return "permission denied". It returns "bad
handle", because there is *no object on the other side*. In a path-based system
you can always name `/etc/shadow` and be told no; the name exists whether you
may use it or not. Here, an object you were not given is not forbidden, it is
**unsayable**. That difference is the whole point of the design.

**Validating pointers is a graph query.** When a program passes a pointer, the
kernel must check the program actually owns that memory before touching it. In
Bramble that check walks the caller's own `Maps` edges — asking the authority on
what is mapped, rather than a copy of it:

```
[user] a pointer into the kernel's half was refused
```

**Names mint capabilities.** `lookup("console")` requires a capability to the
root carrying `Lookup`, and returns a *new capability* in a fresh slot. A name
is not a way to reach something you could not otherwise reach; it is a
convenience for something you were already permitted to ask for.

**And the program can read the entire kernel.** One `inspect` call copies the
whole graph into the program's buffer, which it decodes itself:

```
[user] inspect: 2368 bytes, seq 1120359, 21 nodes, 41 edges, 117413 of 123114 frames free
[user] nodes: 1xRoot 1xCpu 1xProcess 2xThread 2xAddressSpace 13xMemoryObject 1xDevice
[user] edges: 20xOwns 4xHolds 1xInSpace 6xMaps 1xReady 9xNamed
```

2368 bytes for the complete state of an operating system. Not a `/proc` view of
one subsystem; every process, thread, address space, mapping, capability and
name, joined, in one buffer, obtained with one call.

### Killing a process, and why the ownership tree earns its keep here

The second program writes through a null pointer on purpose:

```
*** killing a process: page fault at 0x0 from ring 3 ***
  rip 0x40003d  cause PageFaultErrorCode(CAUSED_BY_WRITE | USER_MODE)
proc: a process died mid-instruction and the graph is still consistent
proc: free frames 117413 before, 117413 after two processes lived and died
```

The handler's entire response is `begin_delete(process)` — detach one edge. The
address space, its page tables, its memory objects, its threads and their
kernel stacks are all owned by that process, directly or transitively, so they
become unreachable at once and the reaper returns them later.

There is no cleanup function. There is no list of things to remember to free.
There is no "did we get everything?" — the frame count answers that, and it does.

A conventional kernel has an explicit teardown path for process exit, it is long,
and it is where leaks live, because it must enumerate by hand what the ownership
tree here states structurally.

### Six bugs, and what they were about

Every one was in conventional machinery. Not one was in the graph.

**1. The kernel ran on an address space it had already destroyed.** After
killing a process the scheduler switched to a kernel thread, which has no
address space of its own, so `cr3` was left pointing at the dead process's page
tables. The reaper then freed those frames. The allocator handed one straight
back as the next process's page-table root, and zeroing it wiped out the
mappings of the code doing the zeroing.

The machine stopped with no output, which is the worst kind of bug. The fix is
one line — a thread with no address space runs in the kernel's — and the lesson
is that "whose page tables am I on right now?" is a question a kernel must be
able to answer at every instant.

**2. A program's own system call destroyed the pointer it was about to use.**
The entry stub saved only the two registers the `syscall` instruction itself
clobbers. But the kernel's handler is an ordinary compiled function, and treats
six more as scratch. The user's compiler, reasonably, kept a live pointer in one
of them across the call.

The symptom was a fault at `0x00007fff00403491` — the top half of a stack
address and the bottom half of a read-only-data address, spliced together. The
kind of value that means a register you trusted was not what you thought.

This is why calling conventions are documented down to the register. Bramble's
is now written down in `abi/src/lib.rs`: only `rcx` and `r11` are clobbered, and
the stub saves the rest.

**3. A kernel stack overflow that failed completely silently.** A helper that
copies an adjacency list into a fixed buffer returns 2 KiB *by value*, and the
`lookup` path used three of them. With 16 KiB of kernel stack, that overflowed.

Kernel stacks live in the direct map — one big mapping of all physical memory —
so there is no unmapped guard page below them to fault on. The overflow walked
into whatever memory happened to be next and the machine stopped.

Three fixes: read-only walks use the borrowing iterator and copy nothing, stacks
are 32 KiB, and every kernel stack now carries a **canary** — a known value at
the very bottom, checked by the same consistency pass that checks everything
else. An overflow is now a named error rather than a mystery.

**4. `swapgs` cannot be paired correctly with Rust's interrupt handlers.** The
standard trick requires that *every* entry from ring 3 swaps and *every* exit
unswaps. Rust's `x86-interrupt` calling convention generates its own prologue,
so a handler written in it cannot swap before the compiler's code runs. A timer
arriving during user code left the two bases crossed, after which the next
system call read its stack pointer from address zero.

v1 drops `gs` entirely: with one core there is no "per-core" anything, so the
stub reads two fixed addresses. Written down as debt — SMP brings the problem
back and needs hand-written interrupt stubs that swap conditionally on the saved
code segment.

**5. Two ELF segments sharing a page.** The linker put a small table between two
segments so that one page contained the end of a read-only segment and the start
of a writable one. Bramble maps one `Maps` edge per segment, and two edges
covering the same virtual page violates invariant I7 — correctly, since the
hardware has only one set of permission bits per page.

Several attempts to make the linker page-align things failed, because that table
is synthesised *after* any linker script has had its say. So the loader was
fixed instead: it computes a protection for each *page* as the union of every
segment touching it, then groups consecutive pages that agree. A real dynamic
loader makes exactly this trade for exactly this reason.

**6. A fault while printing deadlocked the fault handler.** Three of the bugs
above presented as "the machine stops with no output", partly because a fault
taken while the serial lock was held made the crash reporter wait on the code it
was reporting on. The crash paths now break those locks first. Linux calls the
same trick `bust_spinlocks`, and it exists for the same reason.

### The thing worth taking away

Phase 5 was the buggiest phase by a wide margin, and the pattern is consistent
with the previous four: **the graph has caused no bugs. The conventional
machinery around it has caused all of them.**

That is not an argument that the design is free. It costs memory, and it costs a
measurable if small amount of time. But five phases in, the "everything is a
graph" part has been the boring, reliable part, and the register-saving,
`swapgs`-pairing, stack-sizing, page-table-lifetime parts — the parts every
kernel has — have been where the difficulty lives.

---

## What is next

Phase 6 is IPC: two processes exchanging messages over an endpoint, with the
`Waiting` edge flipping between them in the graph as they rendezvous. It carries
the last performance gate, and it is the last point at which the representation
could still change without rewriting userspace.

---

## Entry 7 — Phase 6: two programs that share nothing and still talk

**Milestone: a conversation between two processes, one of which cannot print
until the other hands it the ability to.**

### The concepts

**IPC** — inter-process communication. Two programs are isolated by design;
sooner or later they need to cooperate. Everything above the kernel in a
capability system is built out of this, so it is the operation that matters
most.

**Synchronous rendezvous.** Two designs are possible.

*Asynchronous:* the sender drops a message in a queue and carries on. Needs
buffers, needs a policy for a full queue, and — the reason Bramble does not do
it — a capability sent this way sits *inside a kernel object* while in flight,
somewhere the ownership tree cannot see it.

*Synchronous:* the sender waits until a receiver is there. The message never
exists in a queue; it passes directly from one thread to the other. This is what
seL4 does, and Bramble follows it. `DESIGN.md` settled this in the assumptions
before any code was written, and the reason was exactly the capability-in-flight
problem.

The consequence is pleasing. An **endpoint** has no state of its own at all.
Its entire content is its list of waiting threads — the `Waiting` edges pointing
at it. The whole rendezvous is: look at the head of that list, copy sixty-four
bytes, move one edge to the run queue.

**Capability transfer.** A message can carry a capability. The receiver gets a
*new* edge to the same object; the sender keeps what it had. Rights can only
narrow, because they are masked by what the sender held at the moment it called.

### How programs normally find each other

By name, in a shared global namespace. A Unix socket at `/tmp/something`, a
port number, a D-Bus name. Anyone who can name it can try to connect, and
whether they are allowed to is a separate question answered by a separate
mechanism — file permissions, a firewall, a policy file.

The name is the connection, and the name is public. Which means "who can talk
to this service?" is not a property of the service; it is a property of a
permissions system somewhere else, which someone has to configure correctly.

### What Bramble does

There is no namespace. Two processes can communicate iff each holds a capability
to the same endpoint, and there is no way to obtain one except to be given it.

The demonstration is the ponger. It is spawned holding exactly two capabilities:
one endpoint it may receive on, one it may send on. **It has no console.** It
cannot print, and it cannot acquire the ability to print, because printing means
holding a capability to the console device and it has none and no way to name
one.

Then the pinger sends it one:

```
[pinger] sent the ponger a capability to my console
[ponger] I could not print until this arrived
[ponger] received a console capability in slot 3, first word 0xc0ffee
```

That second line is the point of the whole design. The ponger's ability to
affect the outside world arrived *in a message*, at runtime, from a program that
chose to give it. It was not configured, not inherited, not looked up.

And sending a capability needs `Grant` **on the endpoint**, which is a separate
right from `Send`. Being allowed to talk to someone is not the same as being
allowed to hand them authority. In Unix those are not distinguishable, because
authority is not a thing you hold.

### Watching the conversation from outside

Because a wait is an edge in the same graph as everything else, a third thread
can watch two processes talking:

```
ipc: watched from outside: 62 of 62 samples caught a thread parked on an endpoint
```

Every one of those samples is a moment where one process was blocked on an
endpoint, visible in the same structure that holds processes, memory and
capabilities. In a conventional kernel this is a wait queue inside the IPC
subsystem, and nothing outside that subsystem can see it — which is why
"which processes are waiting on each other?" is not a question you can ask, and
why deadlock detection is a special-purpose tool rather than a graph query.

### The performance gate, and what the number actually means

This phase carried the last of the three gates. Measured in a release build:

| Measurement | Cycles |
|---|---|
| Round trip (4 system calls, 2 rendezvous, several context switches) | 155210 |
| Two null system calls | 1435 |
| Graph work per rendezvous | 7081 |
| — finding the waiter | 1270 |
| — copying the message | 767 |
| — requeueing the partner | 5131 |
| **Graph work as a share of the conversation** | **8%** |

Against a gate of 15%. But it is worth being precise about what 8% is: an
**upper bound on the graph's cost, not a measure of it**. Any kernel doing this
rendezvous has to find a waiter, copy a message and requeue a partner. What the
graph *adds* is the difference between doing that with typed edges and doing it
with two raw pointers, and that is smaller than 8%.

It started at 13%, and two changes closed the gap. Both were the same shape as
phase 4's, and both are worth naming because they are the general lesson:

- **Validating twice.** Every multi-step operation calls `precheck_link` so that
  a failure cannot half-apply, and then calls `link_raw`, which validates the
  same three things again. Splitting the linking half out for callers that have
  just prechecked removed an entire duplicate pass.
- **Re-deriving what is already known.** When an edge is removed, the code did
  per-kind bookkeeping that re-checked what kind of node each endpoint was. But
  the *edge kind already fixes that* — invariant I3 says a `Waiting` edge runs
  from a thread, and the checker verifies it. Asking again cost a node lookup
  every time.

Together those took a rendezvous from 9602 cycles to 7081, and improved the
phase 4 context switch from 4953 to 4101 as a side effect.

The pattern across three phases now: **the graph is not slow, but it is easy to
make it do the same work several times.** Every performance fix so far has been
removing repetition, not removing structure.

### The bug: a global where a per-thread slot was needed

When a program makes a system call, the kernel arrives with the *user's* stack
pointer in `rsp` and has to stash it somewhere before switching to its own
stack. Bramble's stub parked it in a global variable.

That is fine for a system call that returns promptly. It is wrong for one that
**blocks**. `send` on an empty endpoint blocks; another thread runs; that thread
finishes its own system call and returns to ring 3 by loading the user stack
pointer from — the global, which now holds the *other* process's value.

The symptom was a message arriving with a value from one step earlier, and it
moved when unrelated code was added, which is the signature of state shared where
it should not be. I spent several rounds chasing it as a compiler problem before
looking at the stub.

The fix is what real kernels do: the user's stack pointer goes on the
**per-thread kernel stack** with the rest of the saved frame. The global now
holds a value for exactly two instructions, with interrupts masked, before it is
pushed somewhere thread-private.

The general shape is worth keeping: *any* per-CPU or global scratch in a system
call path is a bug waiting for the first call that blocks. This is the same
lesson as phase 5's register-preservation bug — both were "the kernel assumed
its call frame was simpler than it is".

### And a bug in the measuring, not the measured

The first numbers were nonsense: 2.7 million cycles per round trip. Two reasons,
both my fault, both instructive.

The watching thread shares the run queue with the two processes being timed. It
was walking the whole edge set on every scheduling round — so its measurement
work landed *inside* the round-trip time it was measuring. It now checks an
atomic counter every round and samples the graph rarely.

And the first figures came from a debug build, where a context switch costs
88693 cycles against release's 4101. A twenty-fold difference, in numbers being
used to make an architectural decision. **Read the profile before reading the
number.**

---

## What is next

Phase 7 moves process creation out of the kernel: a first user program that
spawns others, hands them capabilities, kills one, and watches its partner's
next message fail cleanly rather than deadlock. Then phase 8 is v1 — the
inspect syscall and the tools that draw the whole system from a snapshot.

---

## Entry 8 — Phase 7: the kernel stops being in charge

**Milestone: the kernel loads one program and then does nothing but clean up
after it.**

### The concept

**init.** Every Unix-like system has one: the first process, started by the
kernel, which starts everything else. On Linux it is `systemd` or similar. The
kernel's role in process creation ends after that one.

Bramble's is smaller than usual, because it holds less. It gets two
capabilities — a console, and the root with `Lookup` — and everything the system
subsequently does is built out of those two.

### How process creation normally works

`fork()` and `exec()`. `fork` makes a copy of the calling process; `exec`
replaces its program. The child inherits *everything*: file descriptors, memory
mappings, the user id, the environment. You then remove what the child should
not have, by closing descriptors and dropping privileges — carefully, in the
right order, remembering everything.

That is inheritance by default with subtraction afterwards, and its failure mode
is the one you would predict: forget a subtraction and the child has authority
nobody meant it to have. Whole categories of security bug live in exactly that
gap, which is why `posix_spawn` and `CLOEXEC` and seccomp filters exist — all of
them ways of getting back to "the child has only what I meant it to have".

### What Bramble does

The child starts with **nothing**, and is given things.

```rust
let child = spawn(image);                                    // exists, not running
grant(child, CONSOLE, R_READ | R_WRITE);                     // may print
grant(child, bootstrap, R_SEND | R_GRANT);                   // may reply to me
start(child);                                                // now it runs
```

Three properties fall out, none of which had to be designed:

**Spawn and start are separate.** A child never runs in a window where its
authority is incomplete, so it does not have to be written to cope with one.

**Spawning needs only the right to read the image.** No special privilege, no
"may create processes" bit. The reason is the ownership tree: the child is owned
by its parent, so everything it consumes is already charged to the parent's
subtree and dies with it. A program cannot escape its own limits by spawning
helpers, because its helpers are inside it.

**`lookup` grants rights that match what was found.** A device comes back
readable and writable. A program image comes back **readable only** — exactly
the authority needed to spawn it and nothing more. Naming a thing does not hand
over control of it.

### Revocation, watched from the inside

The interesting part is the death.

The worker creates an endpoint *it owns* and sends a capability to it back to
init. That is service registration in a capability system: no name to publish,
no registry, just a capability handed to the one party that should have it.

Then init kills the worker. One edge is detached. The worker's process, its
thread, its address space, its page tables, its memory and **its endpoint** all
become unreachable and the reaper returns them.

And init can watch it happen:

```
[init] killing worker 1
[init] after 26 yields, my capability to its endpoint is simply gone
[init] sending through the revoked capability failed cleanly
```

Init does not clean up its own handle. Nothing tells it to. The handle table
entry is cleared by the same operation that removes the `Holds` edge, because
the table is a derived index and the edge is the truth. Init observes an empty
slot where a capability used to be — and then confirms that sending through it
returns an error rather than blocking forever, which is the difference between a
revoked capability and a dangling pointer.

In Unix the equivalent is deleting a file that a process still has open. The
process keeps reading it. The file is gone from the directory but not from the
process, and there is no mechanism to take it back. Revocation is simply not
something Unix can express.

### The proof: a census

The phase compares the shape of the entire graph before and after:

```
life: before  1xRoot 1xCpu 1xAddressSpace 16xMemoryObject 1xDevice 19xOwns 2xMaps 13xNamed | 116397 frames free
life: after   1xRoot 1xCpu 1xAddressSpace 16xMemoryObject 1xDevice 19xOwns 2xMaps 13xNamed | 116397 frames free
```

Identical. Two processes were created, granted capabilities, talked to, killed
and replaced; two endpoints were made and destroyed; a program spawned children
and exited. Not one node, edge or frame was left behind.

The identities all changed — every node has a new generation number — but the
*shape* came back exactly. That is what "the system cleaned up after itself"
looks like when you can state it precisely, and it is a check no conventional
kernel can write about itself, because there is no single structure whose shape
you could compare.

### Two small bugs worth the mention

Only the first four boot modules were being given names, from an early decision
that names were scarce. With six programs, `lookup("worker")` failed. Fixed by
naming all of them — names are edges, and there was never a reason to ration
them.

And `spawn` was still starting the thread it created, so the new separate
`start` failed with `BadState`. Worth noting because of *how* it failed: the
graph's own state machine refused the transition rather than letting a thread be
queued twice. The invariant caught a refactoring mistake, which is the cheapest
possible place to catch one.

---

## What is next

Phase 8 is v1: the final snapshot format, and the host tools that decode it,
draw the whole system, diff two snapshots, and answer "could this process ever
reach that object?" — the take-grant question from 1977, asked of a running
kernel.

---

## Entry 9 — Phase 8: v1, and the whole kernel as bytes

**Milestone: an unprivileged program hands the entire state of the operating
system to the outside world, twice, and a tool on the host draws it, checks it,
diffs it, and answers a question about it that no conventional kernel can be
asked.**

### What v1 was supposed to be

From the first day, before any code: *two userspace processes running
preemptively, communicating over an IPC edge, with the entire kernel state
inspectable as a graph via a syscall.*

All four clauses now hold, and the boot proves each of them every time.

### The concepts

**A snapshot format.** The kernel's state has to leave the machine somehow. The
obvious approach is a text dump, which is what phase 2 built, but text is slow
to produce and lossy to parse. The real interface is binary: a header, a table
of nodes, a table of edges, laid out so a reader can use it directly.

**Compressed sparse row.** The classic way to store a sparse graph. Instead of a
list of (source, target) pairs you sort the edges by source and keep an index
saying where each source's run of edges begins. A reader wanting "everything out
of node 7" reads one index entry and takes a slice — no searching. Bramble's
snapshot is exactly this, and `DESIGN.md` chose it as the export format back in
section 3.4, when the live representation was being decided.

One deviation, and it is deliberate. Within a node's group, edges are in **list
order**, not sorted by target. For `Ready` and `Waiting` that order *is* the
queue: sorting it would throw away the most interesting thing in the snapshot.

**Virtual edges.** "Which thread is running?" is a field on the cpu, not an
edge, because making it an edge would cost two list splices on every context
switch. That was a stated compromise in the design. But a snapshot that omits it
is not "the whole state", so it is emitted as an edge and **flagged as virtual**.
The picture shows it as a dashed line. The compromise is visible rather than
hidden, which is the difference between a principled exception and a hole.

### The result

`v1` is an ordinary program. It holds a console and the root with `Lookup`, and
nothing else. It spawns two workers, talks to them, and then:

```
[v1] the kernel checked its own invariants at my request: all hold
--- snapshot begin quiet bytes=5984 ---
--- snapshot begin one-worker-woken bytes=5984 ---
```

Five thousand nine hundred and eighty-four bytes: fifty nodes, ninety-eight
edges, the complete state of an operating system, obtained with one system call
by a program with no special privilege.

On the host:

```
  offline checker agrees with the kernel: all invariants hold
  between 'quiet' and 'one-worker-woken':
  + edge Ready Cpu#0.1 -> Thread#1.17
  - edge Waiting Thread#1.17 -> Endpoint#1.7 recv
```

Two snapshots, one message apart, and the difference is *exactly* one thread
moving from a wait queue to the run queue. Not "some counters changed" — the
precise structural consequence of one message being sent.

### The question a conventional kernel cannot be asked

The tool implements the take-grant safety query from 1977. Current authority is
one edge; **potential** authority is the closure of every way a capability can
move:

- what a process already holds;
- anything named, if it holds the root with `lookup`;
- anything obtainable by a process it can receive from, when that process can
  also grant over the same endpoint;
- anything its parent could obtain, if the parent may grant into it.

Asked of the running system:

```
Process#0.11 COULD obtain a capability to Root#0.1
Process#0.11 could never obtain a capability to Thread#0.9
```

The second answer shows the query is not vacuous: threads are neither named nor
held by anyone, so no sequence of grants reaches one.

The first is more interesting, and it is a finding rather than a demonstration.
The worker holds nothing but a console and two endpoints. It could nonetheless
obtain the root — because `v1` holds the root with `Lookup`, and holds the
worker with `Grant`, and could therefore pass it along.

That is true, and it says something worth knowing: **holding the root with
`Lookup` is close to unlimited authority**, since the whole namespace is
reachable through it, and a child spawned by such a process is not confined
from it. Nobody reasoned their way to that; the tool was asked and it answered.
The fix is the per-process namespace the design already anticipated — hand a
program the names it needs rather than the root — and now there is a way to
check whether the fix worked.

This is the payoff the whole project was for. Not that the query is clever: it
is a fixed-point computation over a few hundred edges, forty lines of Python.
The point is that **it can be written at all**. It needs a structure that
describes every object, every capability, every channel and every parentage in
one place, with the rules for how authority moves stated over that structure. A
conventional kernel has none of that, not because nobody wanted it, but because
the state is scattered across four unrelated systems that were never designed to
be joined.

### What the picture shows

`build/v1.png`, drawn from the snapshot with no kernel involvement:

- the root, and the `Owns` tree hanging off it — every process, thread, address
  space and page of memory in the system, in one tree;
- three processes with dashed red `Holds` edges to exactly what each may touch,
  labelled with rights and slot numbers;
- the cpu, with a blue `Ready` edge to each queued thread and a dashed orange
  `Running (virtual)` edge to the one executing;
- a thread parked on an endpoint with a dotted `Waiting recv` edge;
- address spaces, and the memory objects mapped into them.

`ps`, `/proc/PID/maps`, `lsof`, a wait-channel dump and a capability audit, in
one image, joined, from one call.

---

## Looking back over eight phases

**The graph caused no bugs.** Not one. Every bug in this log was in the
conventional machinery: a register the system-call stub failed to preserve, a
global where a per-thread slot was needed, a kernel stack sized by guesswork, a
`cr3` left pointing at freed page tables, `swapgs` that cannot be paired with
Rust's interrupt prologue, two ELF segments sharing a page.

**The checker earned its place three times over.** It found three real bugs in
its first minute of existence, including a deadlock the plan expected to meet
six phases later. It caught a refactoring mistake in phase 7 as an illegal state
transition. And it runs on both sides of the system-call boundary now, so the
kernel and the host have to agree.

**But invariants are not correctness.** Phase 4 leaked memory while every
invariant held: stacks were hung off the root rather than off their threads, and
the bookkeeping was perfectly coherent and simply untrue. A dumb before-and-after
frame count caught what the clever machinery could not. Both kinds of check
earn their keep, and they catch different things.

**Every performance fix was removing repetition, not removing structure.** The
round-robin doing ten writes where one would do. `precheck_link` and `link_raw`
validating the same thing twice. `unlink` re-deriving endpoint kinds the edge
kind already fixed. The graph was never slow; it was just asked to do the same
work several times. After those fixes it costs 3% of a context switch and 8% of
an IPC round trip, and the second figure is an upper bound on a cost any kernel
pays some of.

**The design's honesty held up.** `DESIGN.md` pushed back on four of the
original brief's ideas before any code existed, and every one of those pushbacks
turned out to matter: transitive authority would have made confinement
impossible; traversal-based scheduling would have been fatal on the fast path;
reachability-based cleanup would have needed a garbage collector; handles as
nodes would have cost a hop per system call for nothing. The compromises it
listed in section 5.3 are all still there, all still listed, and the one that
was hardest to hold — page tables as a cache of `Maps` edges — is checked in both
directions on every boot.

**What it cost.** About 8x the memory per relationship. A measurable but small
amount of time. Fixed arena sizes. No POSIX, ever. And a persistence story that
does not exist and should not be started as a weekend project.

Whether that trade is worth it depends on what you want from a kernel. If you
want to run existing software fast, obviously not. If you want a system whose
entire state can be handed to a program, drawn, checked, diffed, and asked
questions about that a conventional kernel cannot express — then eight phases in,
the answer looks like yes.

---

## Entry 10 — Phase 9a: the thing the graph is bad at

**Milestone: 512 pages mapped, three pages actually built, and a measurement
showing why a graph needs help with one particular kind of question.**

v1 was reached in the last entry. Everything from here is post-v1, and the
natural place to start is the design's own list of weaknesses.

### The admission

`DESIGN.md` section 5.3 lists seven places where purity was traded for
something. The last one is the least comfortable:

> **Range queries are not native.** "Which mapping covers address v?" needs a
> side index once lazy mapping exists.

That is a real gap, and it is worth being precise about why. A graph relates
**objects**: this process owns that memory, this thread waits on that endpoint.
Adjacency lists answer "what is connected to this?" in one memory read.

But a mapping is not an object relationship, it is an **interval**. Asking
"which of this address space's mappings contains `0x50000000`?" is asking which
of several ranges a point falls in. No adjacency list answers that, because
adjacency is about identity and this question is about ordering. The only thing
the graph can do unaided is look at every mapping in turn.

### The concepts

**A page fault** happens when a program touches an address with no valid
translation. Until now Bramble treated every user fault as fatal: the design
said so explicitly (assumption Q5, *"eager mapping in v1; a user page fault is
always a fault"*), and phase 5 killed a program that wrote through a null
pointer.

But a fault is not necessarily an error. It can be the moment a mapping the
kernel already promised gets *realised*. That is **demand paging**, and it is
how every real system avoids doing work for memory nobody touches.

**Lazy mapping**, the version built here: the `Maps` edge exists — the graph
says the memory is there, the checker verifies it, a snapshot shows it — but no
page-table entries are written. Each page's entry appears when that page is
first touched.

This fits the design's own framing rather neatly. Invariant I5 says *page tables
are a cache of `Maps` edges*. A cache is allowed to be cold. So lazy mapping is
not an exception to the invariant so much as a use of what the invariant already
permits: the graph is the truth, and the hardware catches up.

### What was built

**The index.** Each address space now carries a small array of its own `Maps`
edges, sorted by address. Maintained by the two operations that create and
destroy those edges, and checked by the checker as invariant I11: same count as
the edges, every entry live and belonging to this space, sorted, none missing.

It is a derived index like the handle table and the page tables, and it goes on
the same list in the design's non-graph register, with the same discipline: the
edges are the truth, the index is a shortcut, and something verifies they agree.

A pleasing side effect: the "do these two mappings overlap?" check used to scan
every mapping. In a *sorted* list of disjoint ranges, a new range can only
collide with its immediate neighbours, so it is now two comparisons.

**Three system calls**, so a program manages its own memory: `mem_create`,
`map`, `unmap`. `map` has no argument naming an address space, so it can only
map into the caller's own. Mapping into someone else's would need a capability
to their address space, and nothing in the system hands one out — which is not a
check that had to be written, just a consequence of there being no way to say it.

### The measurement

The question worth answering is not "is the index fast" but "does it beat what
the graph could do unaided". So the scan was kept, and both are measured. 20000
lookups of the worst-case address:

| Mappings | Indexed | Scanning |
|---|---|---|
| 1 | 137 | 137 |
| 4 | 144 | 191 |
| 16 | 182 | 512 |
| 32 | 206 | 958 |

The scan is linear: 7x the cost for 32x the mappings. The index is logarithmic:
1.5x. They are exactly equal at one mapping — a binary search over one element
is a comparison, same as a scan of one element — and the index is ahead from
four onwards.

That low crossover matters. This is not a structure that only pays off at scale,
which would have been an argument for leaving it out of a hobby kernel. It pays
off at four mappings, and a process has five before it does anything.

### The result

```
[lazy] allocated 512 pages (2048 KiB) in slot 2
[lazy] mapped all 512 pages at 0x50000000, lazily: no page-table entries yet
[lazy] touched and verified 3 of 512 pages
[lazy] kernel invariants still hold with the mapping half realised
vm:   3 page faults served by filling in a lazy mapping
```

Three faults for 512 mapped pages. The phase asserts both ends of that: at least
three, because the pages really were written and read back, and fewer than
thirty-two, because otherwise nothing was lazy about it.

### Being honest about what this isn't

The frames are still allocated eagerly and contiguously. Only the *page-table
work* is deferred. Real demand paging allocates the frame on the fault too, and
that needs a `MemoryObject` able to describe a non-contiguous set of pages —
which the design already lists as post-v1, and which changes the shape of a node
rather than adding an index beside it. Copy-on-write needs that plus a reference
count on frames.

So this phase closed the *structural* gap — a range query now has a structure
that answers it, checked like everything else — and left the *allocation*
question for whenever non-contiguous memory objects get built.

### The pattern, one more time

Every phase since the fourth has ended the same way: the graph was not the
problem, but it needed to be asked the right question in the right shape. The
run queue needed an ordered adjacency list. The syscall path needed a slot
table. The fault path needed the page tables. This one needed a sorted array.

None of those are compromises of the thesis, and calling them that would be
sloppy. The thesis is that **one typed graph is the kernel's source of truth**,
not that no other data structure may exist. Every index here is derived from the
graph, maintained by the operations that change the graph, and verified against
the graph by a checker that runs on every boot. What the design refused to do
was let any of them become a second, independent truth — and nine phases in,
none of them has.

---

## Entry 11 — Phase 9b: memory that does not exist until you look at it

**Milestone: two processes sharing memory that neither of them could have
named, at two different addresses, with three pages ever made real out of
sixty-four.**

### The concepts

**Demand allocation.** The previous phase deferred the *page-table* work: the
frames were allocated up front and the hardware learned about them lazily. Real
demand paging defers the memory itself. A program asks for sixty-four pages,
gets a promise, and a page becomes real the first time it is touched.

Almost every program does this. A process asks for a large heap and uses a
fraction of it; a stack is reserved at its maximum and grows into. Allocating
what is promised rather than what is used would waste most of a machine.

**Why this needed a change to a node.** A `MemoryObject` described a
**contiguous** physical range: a base address and a count. That is fine when the
memory is allocated in one go. It cannot describe pages that arrive one at a
time from wherever the allocator had room, because they will not be next to each
other.

So the shape of the node had to change. A paged object now carries the address
of a **frame table** — one physical address per page, zero where the page has
never been touched.

### Where that table lives, and why not in the graph

This is the interesting decision, and the design had already settled the
principle in section 4.4, when it explained why free memory is a bitmap:

> A set is not a relationship. Would be a million edges.

The same argument applies. A frame table is a dense array indexed by position:
page 7 of this object is at *this* address. That is not a relationship between
two objects, it is a lookup keyed by an integer. Modelling it as nodes and edges
would mean a node per page, which for a 64-page object is 64 nodes that say
nothing except "page 7 is here" — and that is what an array element already
says, in eight bytes instead of a hundred.

So the table sits outside the graph, and the graph records where it is. Exactly
what it already does for page tables. And it comes with the same obligation: the
reaper must be told how to free it, which is one more `Reclaim` variant, and
invariant I5 must know how to check it, which is one more branch.

Nine phases in, the pattern is settled and it is worth naming plainly:
**relationships go in the graph; dense arrays indexed by position do not.** The
graph records where the array is, the operations that change the graph maintain
it, and the checker verifies the two agree. Free frames, page tables, handle
tables, the range index and now frame tables all sit on the same side of that
line, for the same reason.

### Sharing, which took no extra code

Once pages come from a table, two processes mapping the same object read the
same table entry and get the same frame. Shared memory was not implemented; it
happened.

```
[share] reserved 64 pages, made 2 of them real, wrote to both
[share] granted the peer read, write and map on that memory, as its slot 3
[peer]  mapped the same memory at 0x66000000, my own choice of address
[peer]  read 0x5eed0001 and 0x5eed0002, wrote 0x5eedbeef back
[share] and what the peer wrote is visible here: 0x5eedbeef
vm:     6 faults across both processes for a 64-page shared region
```

Six faults. Three pages, touched by two processes, at two different virtual
addresses. The object is the shared thing; the address is not, and each process
picked its own.

### How this differs from every other shared memory you have used

In Unix, shared memory has a **name**. `shm_open("/myregion")`, or a System V
key, or a path in `/dev/shm`. The name is how you find it, and the name is in a
namespace that other processes can also see. Whether they may *use* it is a
separate question answered by permission bits somewhere else.

That separation is the problem. Anyone who can name it can attempt it. Access
control is a second mechanism bolted alongside the naming mechanism, and the two
have to be kept in agreement by whoever configures them.

Here the peer reaches those pages because **an edge was created saying it may**,
and for no other reason. There is no key it could have guessed, no path it could
have opened, no namespace to enumerate. Take the edge away and there is nothing
left to try — not "permission denied", but no way to express the request.

That is the same property as phase 5's console capability, and phase 6's
endpoint, and phase 7's revocation. It keeps showing up because it is the one
idea the whole design is made of: **authority is a thing you hold, and if you
were not handed it, it is not merely forbidden, it is unsayable.**

### Being honest about what is still missing

Copy-on-write. Two processes sharing a page and one of them writing should get
its own copy, and that needs a reference count on each frame so the kernel knows
whether a page is shared before it splits it.

That is a change to the frame allocator, not to the graph — the graph already
says who has what — which is a decent sign that the shape is right: the next
feature needs work in the place the feature actually lives.

---

## What is next

The remaining post-v1 list, in the order I would take it:

1. **Async notifications** — a bitmask on `Endpoint`, so an interrupt can signal
   a userspace driver with no thread blocked waiting.
2. **Chunked arena growth**, removing the fixed caps the design accepted for v1.
3. **SMP**, where both recorded debts come due at once: the 8259 interrupt
   controller that does not scale past one core, and the `swapgs` pairing that
   comes back with per-cpu state.
4. **Copy-on-write**, once frames are reference counted.

Persistence stays where the design put it: behind its own document, and not as
a weekend project.
