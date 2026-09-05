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
