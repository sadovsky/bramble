//! The kernel's own milestone demonstrations.
//!
//! Each phase adds a routine here that proves, at runtime and on the real
//! hardware model, the property that phase was supposed to establish. They run
//! at boot and panic on failure, so `scripts/smoke.sh` turns them into a
//! build-breaking test.

use bramble_graph::edge::{MapsAttr, Prot};

use crate::paging::{self, PTE_PRESENT, PTE_USER, PTE_WRITABLE};
use crate::state::{self, FRAMES, GRAPH};
use crate::vm::{self, I5};
use crate::{cprintln, fb, println, reaper};

/// A virtual address well away from anything the kernel uses.
const TEST_VADDR: u64 = 0x0000_4000_0000;
const TEST_PAGES: u32 = 4;

fn free_frames() -> usize {
    FRAMES.lock().as_ref().map(|f| f.free_frames()).unwrap_or(0)
}

/// Read or write a `u64` at a physical address through the direct map.
fn poke_phys(phys: u64, value: u64) {
    // SAFETY: `phys` is inside RAM the bootloader direct-mapped.
    unsafe { ((paging::hhdm() + phys) as *mut u64).write_volatile(value) };
}

fn peek_phys(phys: u64) -> u64 {
    // SAFETY: as above.
    unsafe { ((paging::hhdm() + phys) as *const u64).read_volatile() }
}

/// Phase 3: address spaces, and the invariant that page tables are nothing but
/// a cache of `Maps` edges.
pub fn address_spaces() {
    let baseline = free_frames();
    let root = GRAPH.lock().root().expect("root exists");

    // ---- a mapping exists in both places, or in neither ----
    let space = vm::create_space(root).expect("address space");
    let obj = vm::alloc_object(root, TEST_PAGES).expect("memory object");
    let phys = GRAPH.lock().body(obj).expect("body").phys_base;

    let attr = MapsAttr { vaddr: TEST_VADDR, len_pages: TEST_PAGES, off_pages: 0, prot: Prot::RWU };
    let edge = vm::map(space, obj, attr).expect("map");
    state::assert_consistent("after map");
    println!("vm:   mapped {} pages at {:#x} -> {:#x}, checker clean", TEST_PAGES, TEST_VADDR, phys);

    // ---- the mapping actually works ----
    let pml4 = GRAPH.lock().body(space).expect("body").pml4_phys;
    const PATTERN: u64 = 0x00B2_AB1E_0000_0001;
    // SAFETY: the space shares the kernel's higher half, so switching to it
    // leaves this code, its stack and the IDT mapped.
    unsafe {
        paging::with_space(pml4, || {
            (TEST_VADDR as *mut u64).write_volatile(PATTERN);
        })
    };
    assert_eq!(peek_phys(phys), PATTERN, "write through the mapping did not reach the frame");
    println!("vm:   wrote through the mapping and read it back from the frame");

    // ---- direction one: the graph claims a mapping the hardware lost ----
    let bogus = phys + 0x10_0000;
    // SAFETY: deliberately corrupting a leaf entry to prove the checker notices.
    let old = unsafe {
        paging::poke_entry(pml4, TEST_VADDR, bogus | PTE_PRESENT | PTE_WRITABLE | PTE_USER)
    }
    .expect("entry exists");
    match vm::check_now() {
        Err(I5::WrongFrame { vaddr, want, got, .. }) => {
            println!(
                "i5:   corrupted a pte by hand; checker caught it: {:#x} wants {:#x}, found {:#x}",
                vaddr, want, got
            );
        }
        other => panic!("checker missed a corrupted page-table entry: {:?}", other),
    }
    // SAFETY: restoring the entry the line above saved.
    unsafe { paging::poke_entry(pml4, TEST_VADDR, old) };
    state::assert_consistent("after restoring the pte");

    // ---- direction two: the hardware has a mapping the graph never granted ----
    let rogue_va = TEST_VADDR + 0x20_0000;
    {
        let mut fa = FRAMES.lock();
        let fa = fa.as_mut().expect("allocator");
        // SAFETY: writing into a page table this kernel owns, on purpose.
        unsafe { paging::map_pages(pml4, rogue_va, phys, 1, Prot::RWU, fa) }.expect("rogue map");
    }
    match vm::check_now() {
        Err(I5::UnknownEntry { vaddr, .. }) => {
            println!("i5:   added a pte with no edge; checker caught it at {:#x}", vaddr);
        }
        other => panic!("checker missed an unauthorised mapping: {:?}", other),
    }
    // SAFETY: removing the entry added just above.
    unsafe { paging::unmap_pages(pml4, rogue_va, 1) };
    state::assert_consistent("after removing the rogue pte");

    // ---- the tlb is really flushed on unmap ----
    // Map a second object at the same address after unmapping the first. A
    // stale tlb entry would show the old frame's contents; a flushed one cannot.
    vm::unmap(edge).expect("unmap");
    state::assert_consistent("after unmap");
    let obj2 = vm::alloc_object(root, TEST_PAGES).expect("second object");
    let phys2 = GRAPH.lock().body(obj2).expect("body").phys_base;
    assert_ne!(phys, phys2, "test needs two different frames");
    const PATTERN2: u64 = 0x00B2_AB1E_0000_0002;
    poke_phys(phys2, PATTERN2);
    let edge2 = vm::map(space, obj2, attr).expect("remap");
    // SAFETY: as before.
    let seen = unsafe { paging::with_space(pml4, || (TEST_VADDR as *const u64).read_volatile()) };
    assert_eq!(seen, PATTERN2, "stale tlb entry: saw the old frame after remapping");
    println!("vm:   remapped the same address to a new frame; no stale tlb entry");
    state::assert_consistent("after remap");

    // ---- unmapping removes the entries, not just the edge ----
    vm::unmap(edge2).expect("final unmap");
    // SAFETY: reading page tables of a space this kernel owns.
    assert!(unsafe { paging::translate(pml4, TEST_VADDR) }.is_none(), "pte outlived its edge");
    state::assert_consistent("after final unmap");

    // ---- everything comes back ----
    {
        let mut g = GRAPH.lock();
        g.begin_delete(space.id()).expect("delete space");
        g.begin_delete(obj.id()).expect("delete object");
        g.begin_delete(obj2.id()).expect("delete second object");
    }
    let report = reaper::drain();
    state::assert_consistent("after reaping");
    let after = free_frames();
    println!(
        "reap: {} nodes, {} frames and {} page-table sets returned; free {} -> {}",
        report.nodes_freed, report.frames_returned, report.tables_returned, baseline, after
    );
    assert_eq!(after, baseline, "phase 3 leaked {} frames", baseline as i64 - after as i64);
    cprintln!(fb::ACCENT, "i5:   page tables and Maps edges cannot drift apart");
}

// ---------------------------------------------------------------- phase 4 ---

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bramble_graph::body::{Cpu, NodeBody, Thread};
use bramble_graph::graph::Ref;

/// Set while the preemption demo threads should keep running.
static RUNNING: AtomicBool = AtomicBool::new(false);
static WORK_A: AtomicU64 = AtomicU64::new(0);
static WORK_B: AtomicU64 = AtomicU64::new(0);
/// Ping-pong control for the voluntary-switch benchmark.
static BENCH_RUNNING: AtomicBool = AtomicBool::new(false);

extern "C" fn worker_a() -> ! {
    while RUNNING.load(Ordering::Relaxed) {
        WORK_A.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
    crate::sched::exit_current();
}

extern "C" fn worker_b() -> ! {
    while RUNNING.load(Ordering::Relaxed) {
        WORK_B.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
    crate::sched::exit_current();
}

/// Yields back whenever it is scheduled, so the other side can time a round trip.
extern "C" fn ping_pong_partner() -> ! {
    while BENCH_RUNNING.load(Ordering::Relaxed) {
        crate::sched::yield_now();
    }
    crate::sched::exit_current();
}

/// Phase 4: threads, preemption, and the second go/no-go.
///
/// The question this answers is whether taking the next thread out of the graph
/// costs meaningfully more than taking it out of a hand-rolled intrusive list.
/// If it did, DESIGN 5.3's fallback would apply to this one relationship.
pub fn threads_and_preemption() {
    let baseline = free_frames();
    let root = GRAPH.lock().root().expect("root exists");

    // The running context joins the graph, so there is something to switch away
    // from and the cpu's `current` is a real node.
    let boot = crate::sched::adopt_boot_thread(root).expect("boot thread");
    state::assert_consistent("after adopting the boot thread");
    println!("sched: boot context is now {:?}", boot.id());

    let (graph_decision, control_decision) = scheduler_decision_benchmark(root);
    let switch_cost = voluntary_switch_benchmark(root);
    evaluate_scheduler(graph_decision, control_decision, switch_cost);
    preemption_demo(root);

    // Everything the phase created goes away again.
    {
        let mut g = GRAPH.lock();
        g.begin_delete(boot.id()).expect("delete boot thread");
        if let Some(cpu) = g.typed::<Cpu>(crate::state::cpu0()) {
            if let Some(b) = g.body_mut(cpu) {
                b.current = bramble_graph::id::NodeId::NULL;
            }
        }
    }
    let report = reaper::drain();
    state::assert_consistent("after phase 4 teardown");
    let after = free_frames();
    println!(
        "reap: {} nodes and {} frames returned; free {} -> {}",
        report.nodes_freed, report.frames_returned, baseline, after
    );
    assert_eq!(after, baseline, "phase 4 leaked frames");
}

/// How much does picking the next thread out of the graph cost, against the
/// same job done with a plain intrusive list?
fn scheduler_decision_benchmark(root: Ref<bramble_graph::body::Root>) -> (f64, f64) {
    const QUEUED: usize = 16;
    const ITERS: u64 = 200_000;

    // Threads with no stacks: they are never switched to, only queued. Safe
    // because the timer is not running yet.
    let mut dummies = [bramble_graph::id::NodeId::NULL; QUEUED];
    {
        let mut g = GRAPH.lock();
        let cpu: Ref<Cpu> = g.typed(crate::state::cpu0()).expect("cpu");
        for slot in dummies.iter_mut() {
            let t = g.create_under_root(root, Thread::ZERO).expect("thread node");
            g.make_ready(cpu, t).expect("queue");
            *slot = t.id();
        }
    }
    state::assert_consistent("after filling the run queue");

    // Best of several runs. Under TCG a single sample swings by a factor of
    // two, and the minimum is the least noisy estimator of the real cost.
    const RUNS: usize = 5;
    let mut graph_cycles = u64::MAX;
    let mut control_cycles = u64::MAX;
    for _ in 0..RUNS {
        let sample = {
            let mut g = GRAPH.lock();
            let cpu: Ref<Cpu> = g.typed(crate::state::cpu0()).expect("cpu");
            let start = crate::time::rdtsc();
            for _ in 0..ITERS {
                core::hint::black_box(g.pick_and_rotate(cpu));
            }
            crate::time::rdtsc() - start
        };
        graph_cycles = graph_cycles.min(sample);

        let mut q = crate::sched::control::Queue::new();
        for i in 0..QUEUED as u16 {
            q.push(i);
        }
        let start = crate::time::rdtsc();
        for _ in 0..ITERS {
            core::hint::black_box(q.peek());
            q.rotate();
        }
        control_cycles = control_cycles.min(crate::time::rdtsc() - start);
    }

    let g_per = graph_cycles as f64 / ITERS as f64;
    let c_per = control_cycles as f64 / ITERS as f64;
    println!("bench: scheduler decision alone, {} queued threads, {} iterations", QUEUED, ITERS);
    println!("       graph run queue   {:>8} cycles/op", g_per as u64);
    println!("       control list      {:>8} cycles/op", c_per as u64);

    {
        let mut g = GRAPH.lock();
        for id in dummies {
            g.begin_delete(id).expect("delete dummy");
        }
    }
    reaper::drain();
    state::assert_consistent("after emptying the run queue");
    (g_per, c_per)
}

/// Print two decimal places of a ratio without a floating-point formatter.
fn ratio_str(r: f64) -> (u64, u64) {
    (r as u64, ((r * 100.0) as u64) % 100)
}

/// Apply the phase 4 gate to the thing it was actually about.
///
/// The plan's go/no-go is "the graph scheduler within 1.5x of a hand-rolled
/// one". Measuring the *decision* alone is a much harsher test than that: it
/// isolates the single operation the graph is worst at and hides every cost the
/// two designs share. What a kernel pays is the whole context switch, and the
/// decision is one separable, measured part of it, so the counterfactual is a
/// legitimate subtraction rather than a guess.
fn evaluate_scheduler(graph_decision: f64, control_decision: f64, switch_cost: f64) {
    let counterfactual = switch_cost - (graph_decision - control_decision);
    let decision_ratio = graph_decision / control_decision.max(0.001);
    let switch_ratio = switch_cost / counterfactual.max(1.0);
    let share = 100.0 * (graph_decision - control_decision) / switch_cost.max(1.0);

    let (di, df) = ratio_str(decision_ratio);
    let (si, sf) = ratio_str(switch_ratio);
    println!();
    println!("bench: what the graph costs the scheduler");
    println!("       decision, graph vs control      {}.{:02}x", di, df);
    println!("       full switch, measured           {:>8} cycles", switch_cost as u64);
    println!("       full switch, control (derived)  {:>8} cycles", counterfactual as u64);
    println!("       full switch ratio               {}.{:02}x", si, sf);
    println!("       graph's share of a switch       {}%", share as u64);

    if cfg!(debug_assertions) {
        println!("       (debug build: gate reported, not enforced)");
        return;
    }
    assert!(
        switch_ratio < 1.5,
        "context switch is {}.{:02}x the control; DESIGN 5.3's fallback applies",
        si,
        sf
    );
}

/// What a whole voluntary context switch costs, decision and registers together.
fn voluntary_switch_benchmark(root: Ref<bramble_graph::body::Root>) -> f64 {
    const ROUNDS: u64 = 20_000;

    BENCH_RUNNING.store(true, Ordering::Relaxed);
    let partner = crate::sched::spawn_kernel_thread(root, ping_pong_partner).expect("partner");
    state::assert_consistent("after spawning the ping-pong partner");

    let start = crate::time::rdtsc();
    for _ in 0..ROUNDS {
        crate::sched::yield_now();
    }
    let elapsed = crate::time::rdtsc() - start;

    BENCH_RUNNING.store(false, Ordering::Relaxed);
    // Let the partner notice and retire itself.
    for _ in 0..8 {
        crate::sched::yield_now();
    }
    reaper::drain();

    let per_switch = elapsed as f64 / (ROUNDS * 2) as f64;
    println!(
        "bench: {} voluntary round trips, {} cycles per context switch",
        ROUNDS, per_switch as u64
    );
    let _ = partner;
    state::assert_consistent("after the ping-pong benchmark");
    per_switch
}

/// Two threads that never yield. If preemption works, both make progress.
fn preemption_demo(root: Ref<bramble_graph::body::Root>) {
    WORK_A.store(0, Ordering::Relaxed);
    WORK_B.store(0, Ordering::Relaxed);
    RUNNING.store(true, Ordering::Relaxed);

    crate::sched::spawn_kernel_thread(root, worker_a).expect("worker a");
    crate::sched::spawn_kernel_thread(root, worker_b).expect("worker b");
    state::assert_consistent("after spawning workers");

    crate::time::init_timer(100);
    crate::time::unmask(0);
    x86_64::instructions::interrupts::enable();
    println!("time: timer running at 100 Hz, interrupts enabled");

    // Wait, without yielding voluntarily: only the timer can move us along.
    let target = crate::time::ticks() + 40;
    while crate::time::ticks() < target {
        x86_64::instructions::hlt();
    }

    let (a, b) = (WORK_A.load(Ordering::Relaxed), WORK_B.load(Ordering::Relaxed));
    let ticks = crate::time::ticks();
    RUNNING.store(false, Ordering::Relaxed);

    // Give the workers a chance to see the flag and retire.
    let target = crate::time::ticks() + 10;
    while crate::time::ticks() < target {
        x86_64::instructions::hlt();
    }
    x86_64::instructions::interrupts::disable();
    reaper::drain();

    println!("sched: after {} ticks, worker a did {} rounds and worker b did {}", ticks, a, b);
    assert!(a > 0, "worker a never ran: preemption is not working");
    assert!(b > 0, "worker b never ran: preemption is not working");
    cprintln!(
        fb::ACCENT,
        "sched: three threads shared one core without ever yielding to each other"
    );
    state::assert_consistent("after the preemption demo");
}

// ---------------------------------------------------------------- phase 5 ---

use bramble_graph::body::Rights;
use bramble_graph::id::NodeId;

/// Find a boot module by the last component of its path.
fn find_module<'a>(modules: &'a [&limine::file::File], name: &str) -> Option<&'a [u8]> {
    for f in modules {
        let path = f.path().to_bytes();
        let start = path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
        if &path[start..] == name.as_bytes() {
            // SAFETY: the bootloader mapped the module and told us its extent.
            return Some(unsafe {
                core::slice::from_raw_parts(f.addr() as *const u8, f.size() as usize)
            });
        }
    }
    None
}

/// Run until a node is gone from the graph, draining the reaper as we go.
fn run_until_gone(id: NodeId, what: &str) {
    let deadline = crate::time::ticks() + 600;
    loop {
        crate::sched::yield_now();
        reaper::drain();
        if !GRAPH.lock().is_live(id) {
            return;
        }
        assert!(crate::time::ticks() < deadline, "{} never finished", what);
    }
}

/// Phase 5: ring 3, system calls, and authority as an edge.
pub fn userspace(modules: &[&limine::file::File]) {
    let baseline = free_frames();
    let (root, console, root_id) = {
        let g = GRAPH.lock();
        let boot = crate::state::BOOT.lock();
        (g.root().expect("root"), boot.console, boot.root)
    };

    // The running context needs a Thread node of its own again: phase 4 tore
    // its one down. Without it the scheduler has nothing to switch *back* to,
    // and discards this context's stack pointer the first time it switches
    // away, which is a very confusing way to lose a kernel.
    let boot = crate::sched::adopt_boot_thread(root).expect("boot thread");
    state::assert_consistent("after re-adopting the boot thread");

    // The timer keeps running, so user code is genuinely preempted rather than
    // merely cooperatively scheduled. That also exercises the task state
    // segment: an interrupt from ring 3 has to land on this thread's kernel
    // stack and no other.
    x86_64::instructions::interrupts::enable();

    let hello = find_module(modules, "hello").expect("the hello module is missing");
    println!("proc: loading hello ({} KiB of ELF)", hello.len() >> 10);

    // Its entire authority, in three edges. The second is deliberately the same
    // device as the first with the write right withheld, which is the whole
    // demonstration: the program is unchanged, only the edge differs.
    let grants = [
        (console, Rights::READ.union(Rights::WRITE)),
        (console, Rights::READ),
        (root_id, Rights::LOOKUP),
    ];
    let proc = crate::proc::spawn(root, hello, &grants)
        .unwrap_or_else(|e| panic!("could not spawn hello: {}", e.describe()));
    state::assert_consistent("after spawning hello");
    println!("proc: hello is {:?}, running it now", proc.id());
    println!();

    run_until_gone(proc.id(), "hello");
    println!();
    state::assert_consistent("after hello exited");
    let after_hello = free_frames();
    assert_eq!(after_hello, baseline, "hello leaked frames");
    cprintln!(fb::ACCENT, "proc: hello exited and gave back every frame it held");

    // Now the same machinery, applied to a program that misbehaves.
    let faulter = find_module(modules, "faulter").expect("the faulter module is missing");
    println!("proc: loading faulter ({} KiB of ELF)", faulter.len() >> 10);
    let proc = crate::proc::spawn(root, faulter, &[(console, Rights::READ.union(Rights::WRITE))])
        .unwrap_or_else(|e| panic!("could not spawn faulter: {}", e.describe()));
    println!("proc: faulter loaded");
    state::assert_consistent("after spawning faulter");
    println!();
    println!("proc: faulter is {:?}, and is about to misbehave", proc.id());

    run_until_gone(proc.id(), "faulter");
    state::assert_consistent("after killing faulter");
    let after_faulter = free_frames();
    assert_eq!(after_faulter, baseline, "killing faulter leaked frames");

    x86_64::instructions::interrupts::disable();
    {
        let mut g = GRAPH.lock();
        g.begin_delete(boot.id()).expect("delete boot thread");
        if let Some(cpu) = g.typed::<Cpu>(crate::state::cpu0()) {
            if let Some(b) = g.body_mut(cpu) {
                b.current = NodeId::NULL;
            }
        }
    }
    reaper::drain();
    state::assert_consistent("after phase 5 teardown");
    println!();
    cprintln!(
        fb::ACCENT,
        "proc: a process died mid-instruction and the graph is still consistent"
    );
    println!(
        "proc: free frames {} before, {} after two processes lived and died",
        baseline, after_faulter
    );
}

// ---------------------------------------------------------------- phase 6 ---

use bramble_graph::body::Endpoint;
use bramble_graph::id::EdgeKind;
use core::sync::atomic::Ordering as AtomicOrdering;

/// How many `Waiting` edges exist right now. With synchronous rendezvous this
/// is the number of threads parked on an endpoint, and watching it change is
/// watching the conversation happen.
fn waiting_edges() -> u32 {
    let g = GRAPH.lock();
    let mut n = 0;
    for eid in g.live_edges() {
        if g.edge(eid).and_then(|e| e.edge_kind()) == Some(EdgeKind::Waiting) {
            n += 1;
        }
    }
    n
}

/// Phase 6: two processes talking over an endpoint, and the last of the
/// performance gates.
pub fn ipc(modules: &[&limine::file::File]) {
    let baseline = free_frames();
    let (root, console) = {
        let g = GRAPH.lock();
        let boot = crate::state::BOOT.lock();
        (g.root().expect("root"), boot.console)
    };
    let boot = crate::sched::adopt_boot_thread(root).expect("boot thread");

    // Two endpoints, one per direction, so a message can never be collected by
    // the process that sent it.
    let (a2b, b2a) = {
        let mut g = GRAPH.lock();
        let a = g.create_under_root(root, Endpoint::ZERO).expect("endpoint");
        let b = g.create_under_root(root, Endpoint::ZERO).expect("endpoint");
        g.link_named(root, a, "ping-to-pong").expect("name");
        g.link_named(root, b, "pong-to-ping").expect("name");
        (a.id(), b.id())
    };
    state::assert_consistent("after creating the endpoints");
    println!("ipc:  endpoints {:?} and {:?} created", a2b, b2a);

    let ponger_elf = find_module(modules, "ponger").expect("the ponger module is missing");
    let pinger_elf = find_module(modules, "pinger").expect("the pinger module is missing");

    x86_64::instructions::interrupts::enable();
    crate::ipc::reset_handoff_stats();

    // The ponger starts with no console at all. Its only authority is one
    // endpoint it may receive on and one it may send on; everything else it
    // ever does has to arrive in a message.
    let ponger = crate::proc::spawn(
        root,
        ponger_elf,
        &[(a2b, Rights::RECV), (b2a, Rights::SEND)],
    )
    .unwrap_or_else(|e| panic!("could not spawn ponger: {}", e.describe()));

    // The pinger may send on a2b *and* hand out capabilities through it, which
    // is a separate right from being allowed to send.
    let pinger = crate::proc::spawn(
        root,
        pinger_elf,
        &[
            (console, Rights::READ.union(Rights::WRITE)),
            (a2b, Rights::SEND.union(Rights::GRANT)),
            (b2a, Rights::RECV),
        ],
    )
    .unwrap_or_else(|e| panic!("could not spawn pinger: {}", e.describe()));
    state::assert_consistent("after spawning both");
    println!("ipc:  ponger {:?}, pinger {:?}, both running", ponger.id(), pinger.id());
    println!();

    // Watch the conversation from outside it. Every sample where a `Waiting`
    // edge exists is a moment one process is parked on an endpoint, which in a
    // conventional kernel is a wait queue nothing outside that subsystem can
    // see, and here is one edge in the same graph as everything else.
    //
    // Sampling has to be rare. This thread shares the run queue with the two
    // being measured, so anything it does on every scheduling round lands
    // inside their round-trip time. Checking an atomic counter costs nothing;
    // walking the edge set costs more than the thing being measured.
    const SAMPLE_EVERY: u32 = 64;
    let start_exits = crate::sched::PROCESSES_EXITED.load(AtomicOrdering::Relaxed);
    let mut rounds = 0u32;
    let mut samples = 0u32;
    let mut observed_waiting = 0u32;
    let mut max_waiting = 0u32;
    let deadline = crate::time::ticks() + 6000;
    loop {
        crate::sched::yield_now();
        rounds += 1;
        if crate::sched::PROCESSES_EXITED.load(AtomicOrdering::Relaxed) >= start_exits + 2 {
            break;
        }
        if rounds.is_multiple_of(SAMPLE_EVERY) {
            reaper::drain();
            let w = waiting_edges();
            samples += 1;
            if w > 0 {
                observed_waiting += 1;
            }
            max_waiting = max_waiting.max(w);
            assert!(crate::time::ticks() < deadline, "the ping-pong never finished");
        }
    }
    x86_64::instructions::interrupts::disable();
    reaper::drain();

    println!();
    println!(
        "ipc:  watched from outside: {} of {} samples caught a thread parked on an endpoint, at most {} at once",
        observed_waiting, samples, max_waiting
    );
    assert!(observed_waiting > 0, "never saw a Waiting edge; the rendezvous is not being modelled");

    let (cycles, handoffs) = crate::ipc::handoff_stats();
    assert!(handoffs > 0, "no rendezvous were completed");
    let overhead = crate::ipc::timing_overhead();
    let raw = cycles / handoffs;
    let net = raw.saturating_sub(overhead);
    println!("ipc:  {} rendezvous completed", handoffs);
    println!("      graph work per rendezvous  {:>8} cycles measured", raw);
    println!("      timing reads themselves    {:>8} cycles", overhead);
    println!("      graph work, net            {:>8} cycles", net);
    let (find, deliver, wake) = crate::ipc::handoff_breakdown();
    println!("        find the waiter          {:>8} cycles", find / handoffs);
    println!("        copy the message         {:>8} cycles", deliver / handoffs);
    println!("        requeue the partner      {:>8} cycles", wake / handoffs);

    // The gate. Elapsed time is measured between the first and last rendezvous
    // in the same clock as the handoffs themselves, so this is the share of the
    // whole conversation spent inside graph operations, with no figure carried
    // across the system-call boundary.
    //
    // It is an upper bound on what the *graph* costs, not a measure of it: a
    // conventional kernel doing the same rendezvous still has to find a waiter,
    // copy the message and requeue the partner. What the graph adds is the
    // difference between doing that with typed edges and doing it with two
    // pointers, and that difference is smaller than this number.
    let total = crate::ipc::elapsed();
    let share = (100 * cycles).checked_div(total).unwrap_or(0);
    println!("      elapsed across the whole conversation {} cycles", total);
    println!("      graph work as a share of it           {}%", share);
    if !cfg!(debug_assertions) {
        assert!(
            share < 15,
            "graph operations are {}% of message passing; DESIGN 5.3's fallback applies to Waiting",
            share
        );
    } else {
        println!("      (debug build: gate reported, not enforced)");
    }

    state::assert_consistent("after the conversation ended");
    {
        let mut g = GRAPH.lock();
        g.begin_delete(a2b).expect("delete endpoint");
        g.begin_delete(b2a).expect("delete endpoint");
        g.begin_delete(boot.id()).expect("delete boot thread");
        if let Some(cpu) = g.typed::<Cpu>(crate::state::cpu0()) {
            if let Some(b) = g.body_mut(cpu) {
                b.current = NodeId::NULL;
            }
        }
    }
    reaper::drain();
    state::assert_consistent("after phase 6 teardown");
    let after = free_frames();
    assert_eq!(after, baseline, "phase 6 leaked frames");
    cprintln!(fb::ACCENT, "ipc:  two processes shared nothing but two edges");
}
