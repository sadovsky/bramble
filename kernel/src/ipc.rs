//! Synchronous message passing over an endpoint.
//!
//! A message exists only while two threads are looking at it: the sender parks
//! it on its own `Thread` node and blocks, or hands it straight to a receiver
//! that is already waiting. There is no queue, so there is no node churn per
//! message and no way for a capability to sit "in transit" inside a kernel
//! object where the ownership tree cannot see it (DESIGN Q1).
//!
//! The whole of the rendezvous is three graph operations: look at the head of
//! the endpoint's `Waiting` in-list, move one edge, and copy sixty-four bytes.
//! An endpoint's state *is* its wait list; there is nothing else to it.

use bramble_abi::*;
use bramble_graph::body::{Cpu, Endpoint, Message, Process, Rights, Thread, ThreadState};
use bramble_graph::edge::{WaitRole, WaitingAttr};
use bramble_graph::graph::{Graph, Ref};
use bramble_graph::id::{NodeId, NodeKind};

use core::sync::atomic::{AtomicU64, Ordering};

use crate::sched;
use crate::state::GRAPH;

/// Cycles spent inside the graph operations that make up a rendezvous, and how
/// many rendezvous that covers.
///
/// The two `rdtsc` reads are themselves part of the measurement, so this is an
/// upper bound on the graph's share rather than a precise figure. That is the
/// right direction to be wrong in for a gate.
static HANDOFF_CYCLES: AtomicU64 = AtomicU64::new(0);
static HANDOFFS: AtomicU64 = AtomicU64::new(0);
/// Sub-timings, so the cost can be attributed rather than guessed at.
static FIND_CYCLES: AtomicU64 = AtomicU64::new(0);
static DELIVER_CYCLES: AtomicU64 = AtomicU64::new(0);
static WAKE_CYCLES: AtomicU64 = AtomicU64::new(0);

/// When the conversation started and when it last did anything, so the graph's
/// share can be expressed against elapsed time rather than against a figure
/// measured on the other side of the system-call boundary.
static FIRST_TS: AtomicU64 = AtomicU64::new(0);
static LAST_TS: AtomicU64 = AtomicU64::new(0);

pub fn elapsed() -> u64 {
    LAST_TS.load(Ordering::Relaxed).saturating_sub(FIRST_TS.load(Ordering::Relaxed))
}

#[inline]
fn mark(start: u64, end: u64) {
    let _ = FIRST_TS.compare_exchange(0, start, Ordering::Relaxed, Ordering::Relaxed);
    LAST_TS.store(end, Ordering::Relaxed);
}

pub fn handoff_breakdown() -> (u64, u64, u64) {
    (
        FIND_CYCLES.load(Ordering::Relaxed),
        DELIVER_CYCLES.load(Ordering::Relaxed),
        WAKE_CYCLES.load(Ordering::Relaxed),
    )
}

pub fn handoff_stats() -> (u64, u64) {
    (HANDOFF_CYCLES.load(Ordering::Relaxed), HANDOFFS.load(Ordering::Relaxed))
}

/// What a pair of back-to-back timing reads costs on this machine.
///
/// Under emulation `rdtsc` is itself expensive, and there are two of them
/// inside every measured handoff. Without subtracting this the graph's share
/// would be mostly a measurement of the measurement.
pub fn timing_overhead() -> u64 {
    const N: u64 = 2000;
    let mut total = 0u64;
    for _ in 0..N {
        let a = crate::time::rdtsc();
        let b = crate::time::rdtsc();
        total += b.wrapping_sub(a);
    }
    total / N
}

pub fn reset_handoff_stats() {
    HANDOFF_CYCLES.store(0, Ordering::Relaxed);
    HANDOFFS.store(0, Ordering::Relaxed);
    FIND_CYCLES.store(0, Ordering::Relaxed);
    DELIVER_CYCLES.store(0, Ordering::Relaxed);
    WAKE_CYCLES.store(0, Ordering::Relaxed);
    FIRST_TS.store(0, Ordering::Relaxed);
    LAST_TS.store(0, Ordering::Relaxed);
}

/// Copy a message from one thread to another, carrying any capability with it.
///
/// A transferred capability is a new `Holds` edge on the receiver, never a move
/// of the sender's: the sender keeps what it had unless it revokes it. Rights
/// can only narrow, because they are masked by what the sender held when it
/// called.
fn deliver(g: &mut Graph, from: Ref<Thread>, to: Ref<Thread>) -> Result<u32, i64> {
    let msg = g.body(from).ok_or(E_BADHANDLE)?.msg;
    let to_proc = g.body(to).ok_or(E_BADHANDLE)?.owner_proc;

    let mut slot = 0;
    if msg.has_cap != 0 && !msg.cap_target.is_null() {
        let p: Ref<Process> = g.typed(to_proc).ok_or(E_BADHANDLE)?;
        slot = g.grant_raw(p, msg.cap_target, Rights(msg.cap_rights)).map_err(|_| E_NOSPACE)?;
    }
    let b = g.body_mut(to).ok_or(E_BADHANDLE)?;
    b.msg = msg;
    b.msg.cap_slot = slot;
    Ok(slot)
}

fn cpu(g: &Graph) -> Option<Ref<Cpu>> {
    g.typed(crate::state::cpu0())
}

fn state_of(g: &Graph, t: Ref<Thread>) -> ThreadState {
    g.body(t).map(|b| b.state).unwrap_or(ThreadState::Dying)
}

/// Wait until this thread is no longer blocked.
///
/// The loop is not paranoia. A timer can preempt the thread between blocking
/// and yielding, in which case the scheduler switches away first and the yield
/// below happens later, after the wakeup. Re-reading the state rather than
/// assuming makes both orderings correct.
fn block_until_woken(me: Ref<Thread>) {
    loop {
        let blocked = {
            let g = GRAPH.lock();
            state_of(&g, me) == ThreadState::Blocked
        };
        if !blocked {
            return;
        }
        sched::yield_now();
    }
}

/// A thread whose wait was torn down because the endpoint was destroyed.
fn aborted(g: &Graph, t: Ref<Thread>) -> bool {
    g.body(t).map(|b| b.wait_aborted != 0).unwrap_or(true)
}

fn clear_aborted(g: &mut Graph, t: Ref<Thread>) {
    if let Some(b) = g.body_mut(t) {
        b.wait_aborted = 0;
    }
}

/// Send: hand the message to a waiting receiver, or block until one comes.
pub fn send(
    proc: Ref<Process>,
    ep_id: NodeId,
    words: [u64; 8],
    cap_target: NodeId,
    cap_rights: Rights,
) -> i64 {
    if ep_id.kind() != Some(NodeKind::Endpoint) {
        return E_BADHANDLE;
    }
    let mut g = GRAPH.lock();
    let ep: Ref<Endpoint> = match g.typed(ep_id) {
        Some(e) => e,
        None => return E_BADHANDLE,
    };
    let me: Ref<Thread> = match g.typed(sched::current_locked(&g)) {
        Some(t) => t,
        None => return E_BADHANDLE,
    };
    let _ = proc;

    // Park the message on our own node. This is the only place it lives.
    if let Some(b) = g.body_mut(me) {
        b.msg = Message {
            words,
            cap_target,
            cap_rights: cap_rights.0,
            cap_slot: 0,
            has_cap: u8::from(!cap_target.is_null()),
            _pad: [0; 7],
        };
    }

    // Is someone already waiting to receive? The head of one list answers it.
    let start = crate::time::rdtsc();
    let found = g.first_waiter(ep_id, WaitRole::Recv);
    let t_find = crate::time::rdtsc();
    if let Some(receiver) = found {
        if let Err(e) = deliver(&mut g, me, receiver) {
            return e;
        }
        let t_deliver = crate::time::rdtsc();
        let c = match cpu(&g) {
            Some(c) => c,
            None => return E_BADHANDLE,
        };
        if g.wake(c, receiver).is_err() {
            return E_BADHANDLE;
        }
        let end = crate::time::rdtsc();
        FIND_CYCLES.fetch_add(t_find - start, Ordering::Relaxed);
        DELIVER_CYCLES.fetch_add(t_deliver - t_find, Ordering::Relaxed);
        WAKE_CYCLES.fetch_add(end - t_deliver, Ordering::Relaxed);
        HANDOFF_CYCLES.fetch_add(end - start, Ordering::Relaxed);
        HANDOFFS.fetch_add(1, Ordering::Relaxed);
        mark(start, end);
        return 0;
    }

    // Nobody there. Block on the endpoint and wait to be collected.
    if g.make_blocked(me, ep, WaitingAttr { role: WaitRole::Send, badge: 0 }).is_err() {
        return E_BADHANDLE;
    }
    drop(g);

    block_until_woken(me);

    let mut g = GRAPH.lock();
    if aborted(&g, me) {
        clear_aborted(&mut g, me);
        // The endpoint went away while we were waiting on it. Better an error
        // than a thread that blocks for ever on an object that no longer
        // exists, which is what happens if nobody models this case.
        return E_BADHANDLE;
    }
    0
}

/// Receive: take a waiting sender's message, or block until one arrives.
///
/// Returns the slot a transferred capability landed in, or zero.
pub fn recv(proc: Ref<Process>, ep_id: NodeId) -> Result<(u32, [u64; 8]), i64> {
    if ep_id.kind() != Some(NodeKind::Endpoint) {
        return Err(E_BADHANDLE);
    }
    let mut g = GRAPH.lock();
    let ep: Ref<Endpoint> = g.typed(ep_id).ok_or(E_BADHANDLE)?;
    let me: Ref<Thread> = g.typed(sched::current_locked(&g)).ok_or(E_BADHANDLE)?;
    let _ = proc;

    let start = crate::time::rdtsc();
    let found = g.first_waiter(ep_id, WaitRole::Send);
    let t_find = crate::time::rdtsc();
    if let Some(sender) = found {
        let slot = deliver(&mut g, sender, me)?;
        let t_deliver = crate::time::rdtsc();
        let c = cpu(&g).ok_or(E_BADHANDLE)?;
        g.wake(c, sender).map_err(|_| E_BADHANDLE)?;
        let end = crate::time::rdtsc();
        let words = g.body(me).ok_or(E_BADHANDLE)?.msg.words;
        FIND_CYCLES.fetch_add(t_find - start, Ordering::Relaxed);
        DELIVER_CYCLES.fetch_add(t_deliver - t_find, Ordering::Relaxed);
        WAKE_CYCLES.fetch_add(end - t_deliver, Ordering::Relaxed);
        HANDOFF_CYCLES.fetch_add(end - start, Ordering::Relaxed);
        HANDOFFS.fetch_add(1, Ordering::Relaxed);
        mark(start, end);
        return Ok((slot, words));
    }

    g.make_blocked(me, ep, WaitingAttr { role: WaitRole::Recv, badge: 0 })
        .map_err(|_| E_BADHANDLE)?;
    drop(g);

    block_until_woken(me);

    let mut g = GRAPH.lock();
    if aborted(&g, me) {
        clear_aborted(&mut g, me);
        return Err(E_BADHANDLE);
    }
    let b = g.body(me).ok_or(E_BADHANDLE)?;
    Ok((b.msg.cap_slot, b.msg.words))
}
