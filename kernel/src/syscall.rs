//! The system call boundary.
//!
//! This is where the capability system stops being a diagram and starts being
//! the thing that decides what a program can do. Every argument naming a kernel
//! object is a *slot number*; turning one into an object is `Graph::resolve`,
//! which is three dependent loads and two compares and no traversal at all
//! (DESIGN 5.2). A process cannot name an object it was not given, because
//! there is no syntax for it.

use bramble_abi::*;
use bramble_graph::body::{Device, NodeBody, Process, Rights, Thread};
use bramble_graph::edge::{EdgeAttr, MapsAttr, NamedAttr, Prot};
use bramble_graph::graph::Ref;
use bramble_graph::id::{EdgeKind, NodeKind};

use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::VirtAddr;

use crate::state::GRAPH;

/// Rights bits are duplicated in the ABI crate so userspace need not link the
/// graph. If the two ever drift, capability checks silently change meaning.
const _: () = {
    assert!(R_READ == Rights::READ.0);
    assert!(R_WRITE == Rights::WRITE.0);
    assert!(R_MAP == Rights::MAP.0);
    assert!(R_SEND == Rights::SEND.0);
    assert!(R_RECV == Rights::RECV.0);
    assert!(R_GRANT == Rights::GRANT.0);
    assert!(R_LOOKUP == Rights::LOOKUP.0);
    assert!(R_MANAGE == Rights::MANAGE.0);
};

/// Install the `syscall`/`sysret` machinery.
///
/// The GDT was built in the order these MSRs require: kernel code, kernel data,
/// user data, user code. `SYSCALL` takes its selectors from the low half of
/// `STAR`, `SYSRET` from the high half plus fixed offsets, and neither lets you
/// choose the layout.
pub fn init() {
    let sel = crate::cpu::selectors();
    Star::write(sel.user_code, sel.user_data, sel.kernel_code, sel.kernel_data)
        .expect("gdt is laid out for syscall/sysret");
    LStar::write(VirtAddr::new(syscall_entry as *const () as usize as u64));
    // Clear the interrupt flag on entry. v1 runs system calls with interrupts
    // off: they are all short, and a blocking one switches to another thread
    // whose saved flags turn interrupts back on.
    SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::DIRECTION_FLAG);
    // SAFETY: enabling the syscall instruction now that its target is set.
    unsafe { Efer::update(|f| f.insert(EferFlags::SYSTEM_CALL_EXTENSIONS)) };
}

/// The entry point `syscall` jumps to.
///
/// On arrival `rcx` holds the user's return address, `r11` the user's flags,
/// and `rsp` still points at the *user's* stack. The kernel stack comes from a
/// fixed address rather than through `gs`; `percpu.rs` explains why, and what
/// SMP will cost to undo it.
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // One global scratch word, used for exactly two instructions with
        // interrupts masked, and then moved onto the kernel stack. It must not
        // stay in a global: a system call that blocks lets another thread run
        // and return through `sysret` first, and it would take the wrong
        // process's stack pointer with it. The user's stack pointer belongs on
        // the *per-thread* kernel stack, like every other part of the frame.
        "mov qword ptr [rip + {scratch}], rsp",
        "mov rsp, qword ptr [rip + {kernel_rsp}]",
        "push qword ptr [rip + {scratch}]",  // user rsp, now per-thread
        "push r11",                  // user rflags
        "push rcx",                  // user rip
        // Everything the caller is entitled to get back. `dispatch` is an
        // ordinary C function: it preserves rbx, rbp and r12-r15 by the ABI,
        // and treats these six as scratch. The user's compiler assumes they
        // survive, because our ABI says only rcx and r11 are clobbered. Leaving
        // them out means a program's own system call quietly destroys the
        // pointer it was about to use, which is exactly what happened.
        "push rdi",
        "push rsi",
        "push rdx",
        "push r8",
        "push r9",
        "push r10",
        // Nine pushes leaves the stack 8 mod 16; `call` needs it 0 mod 16 so
        // the callee sees the 8 the ABI promises.
        "sub rsp, 8",
        // Our ABI passes arguments in rdi, rsi, rdx, r10 with the call number
        // in rax; the C ABI wants rdi, rsi, rdx, rcx, r8. Two moves bridge them,
        // and both destinations are already saved above.
        "mov rcx, r10",
        "mov r8, rax",
        "call {dispatch}",
        "add rsp, 8",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rcx",
        "pop r11",
        "pop rsp",
        "sysretq",
        scratch = sym crate::percpu::BRAMBLE_USER_RSP,
        kernel_rsp = sym crate::percpu::BRAMBLE_KERNEL_RSP,
        dispatch = sym dispatch,
    );
}

/// Is `[addr, addr + len)` backed by a mapping the caller may use this way?
///
/// "Is this user pointer valid?" is a graph query here, and that is the honest
/// answer rather than a convenience: the `Maps` edges are the authority on what
/// is mapped, so asking them is asking the truth rather than a copy of it. It
/// is O(mappings), which is fine on a path that is already a mode switch.
fn user_range_ok(len: u64, addr: u64, need: Prot) -> bool {
    if len == 0 {
        return true;
    }
    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    // The user half only. A kernel address here is either a bug or an attack.
    if end > 0x0000_8000_0000_0000 {
        return false;
    }
    let g = GRAPH.lock();
    let thread: Ref<Thread> = match g.typed(crate::sched::current_locked(&g)) {
        Some(t) => t,
        None => return false,
    };
    let space = match g.space_of(thread) {
        Some(s) => s,
        None => return false,
    };
    // Every byte must be covered, and a range may span several mappings.
    let mut cursor = addr;
    while cursor < end {
        let mut advanced = false;
        for eid in g.out_edges(space.id(), EdgeKind::Maps) {
            let edge = match g.edge(eid) {
                Some(e) => e,
                None => continue,
            };
            let attr = MapsAttr::decode(edge.data);
            if cursor >= attr.vaddr && cursor < attr.end() && attr.prot.contains(need) {
                cursor = attr.end();
                advanced = true;
                break;
            }
        }
        if !advanced {
            return false;
        }
    }
    true
}

/// Read a user byte slice after checking it is mapped readable.
///
/// # Safety
/// The caller must have validated the range with `user_range_ok` first; this
/// function does that itself, so it is safe to call, but the returned slice is
/// only valid while the address space is not changed underneath it.
fn user_slice(addr: u64, len: u64, need: Prot) -> Option<&'static [u8]> {
    if !user_range_ok(len, addr, need) {
        return None;
    }
    // SAFETY: the range was just verified to be mapped in the current address
    // space with the required access, and v1 does not change mappings while a
    // system call from that address space is in flight.
    Some(unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) })
}

fn user_slice_mut(addr: u64, len: u64) -> Option<&'static mut [u8]> {
    if !user_range_ok(len, addr, Prot::WRITE) {
        return None;
    }
    // SAFETY: as above, and the range is writable.
    Some(unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len as usize) })
}

/// The calling process, from the current thread's owner cache.
fn caller() -> Option<Ref<Process>> {
    let g = GRAPH.lock();
    let t: Ref<Thread> = g.typed(crate::sched::current_locked(&g))?;
    let owner = g.body(t)?.owner_proc;
    g.typed(owner)
}

/// `a3` is unused by v1's calls; it is in the signature because the entry
/// stub always passes four arguments and phase 6's `send` will want it.
extern "C" fn dispatch(a0: u64, a1: u64, a2: u64, _a3: u64, nr: u64) -> i64 {
    match nr {
        SYS_EXIT => crate::sched::exit_current_process(a0 as i32),
        SYS_YIELD => {
            crate::sched::yield_now();
            0
        }
        SYS_WRITE => sys_write(a0, a1, a2),
        SYS_LOOKUP => sys_lookup(a0, a1),
        SYS_INSPECT => sys_inspect(a0, a1),
        SYS_RIGHTS => sys_rights(a0),
        SYS_SEND => sys_send(a0, a1, a2),
        SYS_RECV => sys_recv(a0, a1),
        SYS_SPAWN => sys_spawn(a0),
        SYS_GRANT => sys_grant(a0, a1, a2),
        SYS_START => sys_start(a0),
        SYS_KILL => sys_kill(a0),
        SYS_ENDPOINT => sys_endpoint(),
        SYS_CHECK => sys_check(),
        _ => E_BADCALL,
    }
}

/// Write to a device. The only authority check is one `Holds` edge with the
/// `Write` right; there is no ambient permission to fall back on.
fn sys_write(slot: u64, ptr: u64, len: u64) -> i64 {
    let proc = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let target = {
        let g = GRAPH.lock();
        match g.resolve(proc, slot as u32, Rights::WRITE) {
            Ok(id) => id,
            Err(bramble_graph::graph::GraphError::MissingRights { .. }) => return E_PERM,
            Err(_) => return E_BADHANDLE,
        }
    };
    if target.kind() != Some(NodeKind::Device) {
        return E_BADHANDLE;
    }
    let bytes = match user_slice(ptr, len, Prot::READ) {
        Some(b) => b,
        None => return E_FAULT,
    };
    let io_base = {
        let g = GRAPH.lock();
        let d: Ref<Device> = match g.typed(target) {
            Some(d) => d,
            None => return E_BADHANDLE,
        };
        g.body(d).map(|b| b.io_base).unwrap_or(0)
    };
    crate::serial::write_bytes(io_base, bytes);
    crate::fb::write_bytes(bytes);
    len as i64
}

/// The one name query the kernel offers. Requires a capability to the root
/// carrying `Lookup`, which in practice only the first process is given.
fn sys_lookup(ptr: u64, len: u64) -> i64 {
    let proc = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let name_bytes = match user_slice(ptr, len, Prot::READ) {
        Some(b) => b,
        None => return E_FAULT,
    };
    if name_bytes.len() > bramble_graph::limits::MAX_NAME_LEN {
        return E_NOTFOUND;
    }
    let name = match core::str::from_utf8(name_bytes) {
        Ok(s) => s,
        Err(_) => return E_NOTFOUND,
    };

    let mut g = GRAPH.lock();
    // Authority first: holding the root with Lookup is what permits this.
    let mut allowed = false;
    for eid in g.out_edges(proc.id(), EdgeKind::Holds) {
        let e = match g.edge(eid) {
            Some(e) => e,
            None => continue,
        };
        if e.dst.kind() == Some(NodeKind::Root)
            && bramble_graph::edge::HoldsAttr::decode(e.data).rights.contains(Rights::LOOKUP)
        {
            allowed = true;
            break;
        }
    }
    if !allowed {
        return E_PERM;
    }

    let found = {
        let root = match g.root() {
            Some(r) => r,
            None => return E_NOTFOUND,
        };
        let mut found = None;
        for eid in g.out_edges(root.id(), EdgeKind::Named) {
            if let Some(e) = g.edge(eid) {
                if NamedAttr::decode(e.data).matches(name) {
                    found = Some(e.dst);
                    break;
                }
            }
        }
        match found {
            Some(f) => f,
            None => return E_NOTFOUND,
        }
    };

    // A lookup grants a capability, so it has to say what rights it grants.
    // Read and write only: naming something must not hand over control of it.
    // Naming something must not hand over control of it. A device is readable
    // and writable; a memory object holding a program image is readable only,
    // which is exactly the authority needed to spawn it and no more.
    let rights = match found.kind() {
        Some(NodeKind::Device) => Rights::READ.union(Rights::WRITE),
        Some(NodeKind::MemoryObject) => Rights::READ,
        Some(NodeKind::Endpoint) => Rights::SEND.union(Rights::RECV),
        _ => Rights::READ,
    };
    match g.grant_raw(proc, found, rights) {
        Ok(slot) => slot as i64,
        Err(bramble_graph::graph::GraphError::Incompatible { .. }) => E_BADKIND,
        Err(_) => E_NOSPACE,
    }
}

/// What rights does this slot carry? Lets a program discover what it may do
/// rather than finding out by failing.
fn sys_rights(slot: u64) -> i64 {
    let proc = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let g = GRAPH.lock();
    match g.rights_of(proc, slot as u32) {
        Some(r) => r.0 as i64,
        None => E_BADHANDLE,
    }
}

/// Resolve a slot that must name a process, with the given right.
fn resolve_process(
    proc: Ref<Process>,
    slot: u64,
    need: Rights,
) -> core::result::Result<Ref<Process>, i64> {
    let g = GRAPH.lock();
    let id = match g.resolve(proc, slot as u32, need) {
        Ok(id) => id,
        Err(bramble_graph::graph::GraphError::MissingRights { .. }) => return Err(E_PERM),
        Err(_) => return Err(E_BADHANDLE),
    };
    if id.kind() != Some(NodeKind::Process) {
        return Err(E_BADKIND);
    }
    g.typed(id).ok_or(E_BADHANDLE)
}

/// Create a process from an image the caller can read.
///
/// The authority to spawn is the authority to read the image, and nothing else
/// is needed: the new process is *owned by its parent*, so everything it
/// consumes is already accounted to the parent's subtree and dies with it. A
/// program that spawns cannot outrun its own quota by proxy.
fn sys_spawn(image_slot: u64) -> i64 {
    let parent = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let (phys, pages) = {
        let g = GRAPH.lock();
        let id = match g.resolve(parent, image_slot as u32, Rights::READ) {
            Ok(id) => id,
            Err(bramble_graph::graph::GraphError::MissingRights { .. }) => return E_PERM,
            Err(_) => return E_BADHANDLE,
        };
        if id.kind() != Some(NodeKind::MemoryObject) {
            return E_BADKIND;
        }
        match g.typed::<bramble_graph::body::MemoryObject>(id).and_then(|m| g.body(m)) {
            Some(b) => (b.phys_base, b.pages),
            None => return E_BADHANDLE,
        }
    };
    // SAFETY: a memory object the caller holds, reached through the direct map.
    let image = unsafe {
        core::slice::from_raw_parts(
            (crate::paging::hhdm() + phys) as *const u8,
            pages as usize * 4096,
        )
    };
    let child = match crate::proc::spawn(crate::proc::Owner::Process(parent), image, &[]) {
        Ok(c) => c,
        Err(crate::proc::SpawnError::Elf(_)) => return E_BADIMAGE,
        Err(_) => return E_NOSPACE,
    };
    // The parent gets a capability to what it just made, with full rights over
    // it. That is the only handle to the child in existence.
    let mut g = GRAPH.lock();
    match g.grant(parent, child, Rights::ALL) {
        Ok(slot) => slot as i64,
        Err(_) => E_NOSPACE,
    }
}

/// Copy one of the caller's capabilities into another process, narrowed.
fn sys_grant(proc_slot: u64, cap_slot: u64, mask: u64) -> i64 {
    let parent = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let child = match resolve_process(parent, proc_slot, Rights::GRANT) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut g = GRAPH.lock();
    match g.copy_cap(parent, cap_slot as u32, child, Rights(mask as u32)) {
        Ok(slot) => slot as i64,
        Err(bramble_graph::graph::GraphError::NoFreeSlot) => E_NOSPACE,
        Err(_) => E_BADHANDLE,
    }
}

fn sys_start(proc_slot: u64) -> i64 {
    let parent = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let child = match resolve_process(parent, proc_slot, Rights::MANAGE) {
        Ok(c) => c,
        Err(e) => return e,
    };
    match crate::proc::start(child) {
        Ok(()) => 0,
        Err(_) => E_BADHANDLE,
    }
}

/// Destroy a process. One edge is detached; everything it owns follows.
fn sys_kill(proc_slot: u64) -> i64 {
    let parent = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let child = match resolve_process(parent, proc_slot, Rights::MANAGE) {
        Ok(c) => c,
        Err(e) => return e,
    };
    let mut g = GRAPH.lock();
    match g.begin_delete(child.id()) {
        Ok(()) => 0,
        Err(_) => E_BADHANDLE,
    }
}

/// Make an endpoint. It is owned by the caller, so it dies with the caller and
/// every capability to it is revoked at that moment.
fn sys_endpoint() -> i64 {
    let proc = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let mut g = GRAPH.lock();
    let ep = match g.create_under_process(proc, bramble_graph::body::Endpoint::ZERO) {
        Ok(e) => e,
        Err(_) => return E_NOSPACE,
    };
    match g.grant(proc, ep, Rights::ALL) {
        Ok(slot) => slot as i64,
        Err(_) => E_NOSPACE,
    }
}

/// Read the eight message words out of user memory.
fn read_words(ptr: u64) -> Option<[u64; MSG_WORDS]> {
    let bytes = user_slice(ptr, (MSG_WORDS * 8) as u64, Prot::READ)?;
    let mut words = [0u64; MSG_WORDS];
    for (i, w) in words.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
        *w = u64::from_le_bytes(b);
    }
    Some(words)
}

fn write_words(ptr: u64, words: &[u64; MSG_WORDS]) -> bool {
    match user_slice_mut(ptr, (MSG_WORDS * 8) as u64) {
        Some(bytes) => {
            for (i, w) in words.iter().enumerate() {
                bytes[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
            }
            true
        }
        None => false,
    }
}

fn sys_send(ep_slot: u64, words_ptr: u64, cap_slot: u64) -> i64 {
    let proc = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    let words = match read_words(words_ptr) {
        Some(w) => w,
        None => return E_FAULT,
    };
    let (ep, cap_target, cap_rights) = {
        let g = GRAPH.lock();
        let ep = match g.resolve(proc, ep_slot as u32, Rights::SEND) {
            Ok(id) => id,
            Err(bramble_graph::graph::GraphError::MissingRights { .. }) => return E_PERM,
            Err(_) => return E_BADHANDLE,
        };
        // Passing a capability needs `Grant` on the endpoint: the right to send
        // is not by itself the right to hand out authority.
        if cap_slot != 0 {
            match g.rights_of(proc, ep_slot as u32) {
                Some(r) if r.contains(Rights::GRANT) => {}
                Some(_) => return E_PERM,
                None => return E_BADHANDLE,
            }
            let target = match g.resolve(proc, cap_slot as u32, Rights::NONE) {
                Ok(t) => t,
                Err(_) => return E_BADHANDLE,
            };
            let rights = match g.rights_of(proc, cap_slot as u32) {
                Some(r) => r,
                None => return E_BADHANDLE,
            };
            (ep, target, rights)
        } else {
            (ep, bramble_graph::id::NodeId::NULL, Rights::NONE)
        }
    };
    crate::ipc::send(proc, ep, words, cap_target, cap_rights)
}

fn sys_recv(ep_slot: u64, words_ptr: u64) -> i64 {
    let proc = match caller() {
        Some(p) => p,
        None => return E_BADHANDLE,
    };
    if !user_range_ok((MSG_WORDS * 8) as u64, words_ptr, Prot::WRITE) {
        return E_FAULT;
    }
    let ep = {
        let g = GRAPH.lock();
        match g.resolve(proc, ep_slot as u32, Rights::RECV) {
            Ok(id) => id,
            Err(bramble_graph::graph::GraphError::MissingRights { .. }) => return E_PERM,
            Err(_) => return E_BADHANDLE,
        }
    };
    match crate::ipc::recv(proc, ep) {
        Ok((slot, words)) => {
            if !write_words(words_ptr, &words) {
                return E_FAULT;
            }
            slot as i64
        }
        Err(e) => e,
    }
}

/// Run the kernel's own consistency check on demand.
///
/// Exposing this to userspace is the point of having one structure with stated
/// rules: any program can ask the kernel to prove it is internally consistent,
/// and get an answer rather than a promise.
fn sys_check() -> i64 {
    match crate::state::check_now() {
        Ok(()) => 0,
        Err(_) => E_BADKIND,
    }
}

/// The whole kernel state, into a user buffer.
fn sys_inspect(ptr: u64, len: u64) -> i64 {
    let g = GRAPH.lock();
    let needed = crate::inspect::snapshot_size(&g);
    if (len as usize) < needed {
        return needed as i64;
    }
    drop(g);
    let buf = match user_slice_mut(ptr, needed as u64) {
        Some(b) => b,
        None => return E_FAULT,
    };
    let g = GRAPH.lock();
    let written = crate::inspect::write_snapshot(&g, buf);
    written as i64
}
