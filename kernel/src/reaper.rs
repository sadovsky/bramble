//! Draining the graph's pending deletions.
//!
//! `Graph::reap_step` does one bounded unit of work and reports what it freed;
//! the graph never touches physical memory itself. This module is the other
//! half: it returns frames and page tables to the allocator and notices threads
//! whose wait was torn down under them.
//!
//! In v1 this runs from the boot path and, once phase 4 lands, from a kernel
//! thread. It must never run in an interrupt handler, which is the whole reason
//! deletion is two-phase (DESIGN 3.8 rule 3).

use bramble_graph::body::MemFlags;
use bramble_graph::graph::{ReapStep, Reclaim};
use bramble_graph::id::NodeId;

use crate::paging;
use crate::state::{FRAMES, GRAPH};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub nodes_freed: u32,
    pub frames_returned: u64,
    pub tables_returned: u32,
    pub waits_aborted: u32,
    /// The last thread stranded by a destroyed wait target. Phase 6 resumes
    /// these with an error; for now the count is what matters.
    pub last_aborted: NodeId,
}

/// Do one step. Returns false when there is nothing left to do.
pub fn step(report: &mut ReapReport) -> bool {
    let outcome = {
        let mut g = GRAPH.lock();
        g.reap_step()
    };
    match outcome {
        ReapStep::Idle => false,
        ReapStep::Progress => true,
        ReapStep::AbortedWait { thread } => {
            report.waits_aborted += 1;
            report.last_aborted = thread;
            true
        }
        ReapStep::Freed { reclaim, .. } => {
            report.nodes_freed += 1;
            match reclaim {
                Reclaim::Nothing => {}
                Reclaim::Frames { phys, pages, flags } => {
                    // Device memory was never ours, and pinned memory is the
                    // kernel's own. Neither goes back to the pool.
                    if !flags.contains(MemFlags::DEVICE) && !flags.contains(MemFlags::PINNED) {
                        if let Some(fa) = FRAMES.lock().as_mut() {
                            fa.free_contiguous(phys, pages as usize);
                            report.frames_returned += pages as u64;
                        }
                    }
                }
                Reclaim::PagedMemory { table_phys, table_pages, pages } => {
                    // Walk the table and give back whichever pages were ever
                    // touched. The graph knows the object had these pages; only
                    // the table knows which of them were made real.
                    if let Some(fa) = FRAMES.lock().as_mut() {
                        for i in 0..pages as u64 {
                            // SAFETY: the table belongs to an object being
                            // reaped, and `i` is inside it.
                            let frame = unsafe { crate::vm::frame_entry(table_phys, i) };
                            if frame != 0 {
                                fa.free_contiguous(frame, 1);
                                report.frames_returned += 1;
                            }
                        }
                        fa.free_contiguous(table_phys, table_pages as usize);
                        report.frames_returned += table_pages as u64;
                    }
                }
                Reclaim::PageTables { pml4_phys } => {
                    if let Some(fa) = FRAMES.lock().as_mut() {
                        // SAFETY: the address space is unreachable and nothing
                        // is running on it; phase one of deletion saw to that.
                        unsafe { paging::free_user_tables(pml4_phys, fa) };
                        report.tables_returned += 1;
                    }
                }
            }
            true
        }
    }
}

/// Run to completion. Bounded only by the size of the dying subtree.
pub fn drain() -> ReapReport {
    let mut report = ReapReport::default();
    let mut guard = 0u32;
    while step(&mut report) {
        guard += 1;
        assert!(guard < 1_000_000, "reaper made no progress");
    }
    report
}
