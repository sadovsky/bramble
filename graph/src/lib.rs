//! Bramble's kernel-state graph.
//!
//! One typed directed graph holds everything the kernel knows: processes,
//! threads, address spaces, memory objects, endpoints, devices, capabilities,
//! ownership, names and scheduler queues. See `docs/DESIGN.md`.
//!
//! The crate is `no_std`, allocation-free and `forbid(unsafe_code)`. All
//! storage is static arenas whose all-zero state is a valid empty graph, which
//! is what lets the graph exist before the allocator does.

#![no_std]
#![forbid(unsafe_code)]

pub mod body;
pub mod checker;
pub mod edge;
pub mod graph;
pub mod id;
pub mod limits;
pub mod slab;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

pub use body::{
    AddressSpace, Cpu, Device, DeviceClass, Endpoint, MemFlags, MemoryObject, Message, NodeBody,
    Process, Rights, Root, Thread, ThreadState,
};
pub use checker::{Checker, Violation};
pub use edge::{
    Dir, Edge, EdgeAttr, HoldsAttr, MapsAttr, NamedAttr, Prot, RawEdgeData, ReadyAttr, WaitRole,
    WaitingAttr,
};
pub use graph::{Graph, GraphError, ReapStep, Ref, Result};
pub use id::{compatible, EdgeId, EdgeKind, NodeId, NodeKind};
