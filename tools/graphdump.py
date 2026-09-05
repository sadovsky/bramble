#!/usr/bin/env python3
"""Read Bramble's kernel state out of a serial log, then draw it, check it,
diff it, or ask what a process could ever reach.

Two formats arrive over the same wire.

  * The kernel's own text dump, emitted between `--- graph begin` markers. It
    exists so the kernel can report its state before userspace exists.
  * A binary snapshot produced by the `inspect` system call and spelled out in
    hexadecimal by the program that asked for it. This is the real interface:
    an ordinary program, holding nothing special, hands out the entire state of
    the operating system as bytes.

Both decode to the same structure, so everything below works on either.

    python3 tools/graphdump.py build/serial.log --png build/graph.png
    python3 tools/graphdump.py build/serial.log --check
    python3 tools/graphdump.py build/serial.log --diff
    python3 tools/graphdump.py build/serial.log --reach Process#1.3 Root#0.1
"""
import argparse
import re
import shutil
import struct
import subprocess
import sys
from dataclasses import dataclass, field

# ------------------------------------------------------------------ format ---

INSPECT_MAGIC = 0x4272616D  # "Bram"

NODE_KINDS = [
    "Root", "Cpu", "Process", "Thread",
    "AddressSpace", "MemoryObject", "Endpoint", "Device",
]
EDGE_KINDS = ["Owns", "Holds", "InSpace", "Maps", "Ready", "Waiting", "Named", "Running"]

RIGHTS = [
    (1 << 0, "R", "read"), (1 << 1, "W", "write"), (1 << 2, "M", "map"),
    (1 << 3, "s", "send"), (1 << 4, "r", "recv"), (1 << 5, "g", "grant"),
    (1 << 6, "l", "lookup"), (1 << 7, "A", "manage"),
]
R_SEND, R_RECV, R_GRANT, R_LOOKUP = 1 << 3, 1 << 4, 1 << 5, 1 << 6

EDGE_FLAG_VIRTUAL = 1 << 0

BEGIN_TEXT = re.compile(r"--- graph begin seq=(\d+) nodes=(\d+) edges=(\d+) ---")
END_TEXT = "--- graph end ---"
NODE_LINE = re.compile(r"^node (\w+)#(\d+)\.(\d+) (\S+)\s*(.*)$")
EDGE_LINE = re.compile(r"^edge (\w+) (\w+)#(\d+)\.(\d+) -> (\w+)#(\d+)\.(\d+)\s*(.*)$")
BEGIN_HEX = re.compile(r"--- snapshot begin (\S+) bytes=(\d+) ---")
END_HEX = "--- snapshot end ---"
HEX_LINE = re.compile(r"^[0-9a-f]{2,64}$")


def rights_str(bits):
    s = "".join(ch for bit, ch, _ in RIGHTS if bits & bit)
    return s or "-"


@dataclass
class Node:
    kind: str
    idx: int
    gen: int
    flags: str = "-"
    attrs: str = ""

    @property
    def key(self):
        return (self.kind, self.idx, self.gen)

    @property
    def label(self):
        return f"{self.kind}#{self.idx}.{self.gen}"


@dataclass
class Edge:
    kind: str
    src: tuple
    dst: tuple
    attrs: str = ""
    virtual: bool = False

    @property
    def key(self):
        return (self.kind, self.src, self.dst, self.attrs)


@dataclass
class Snapshot:
    label: str = "text"
    seq: int = 0
    declared_nodes: int = 0
    declared_edges: int = 0
    free_frames: int = 0
    total_frames: int = 0
    ticks: int = 0
    nodes: dict = field(default_factory=dict)
    edges: list = field(default_factory=list)
    # process key -> {slot: (target key, rights bits)}
    handles: dict = field(default_factory=dict)


def node_id(raw):
    """Unpack a NodeId: kind in the low byte, index, then generation."""
    return (NODE_KINDS[raw & 0xFF] if (raw & 0xFF) < len(NODE_KINDS) else "?",
            (raw >> 8) & 0xFFFFFF,
            raw >> 32)


# ------------------------------------------------------------- text format ---

def parse_text(text):
    snaps, cur = [], None
    for line in text.splitlines():
        line = line.strip()
        m = BEGIN_TEXT.search(line)
        if m:
            cur = Snapshot("text", int(m.group(1)), int(m.group(2)), int(m.group(3)))
            continue
        if cur is None:
            continue
        if line.startswith(END_TEXT):
            snaps.append(cur)
            cur = None
            continue
        m = NODE_LINE.match(line)
        if m:
            n = Node(m.group(1), int(m.group(2)), int(m.group(3)), m.group(4), m.group(5))
            cur.nodes[n.key] = n
            continue
        m = EDGE_LINE.match(line)
        if m:
            src = (m.group(2), int(m.group(3)), int(m.group(4)))
            dst = (m.group(5), int(m.group(6)), int(m.group(7)))
            e = Edge(m.group(1), src, dst, m.group(8))
            cur.edges.append(e)
            if e.kind == "Holds":
                mm = re.search(r"rights=(\S+) slot=(\d+)", e.attrs)
                if mm:
                    bits = sum(b for b, ch, _ in RIGHTS if ch in mm.group(1))
                    cur.handles.setdefault(src, {})[int(mm.group(2))] = (dst, bits)
    return snaps


# ----------------------------------------------------------- binary format ---

def parse_hex(text):
    """Decode every hex-dumped `inspect` snapshot in the log."""
    snaps, blob, label = [], None, ""
    for line in text.splitlines():
        line = line.strip()
        m = BEGIN_HEX.search(line)
        if m:
            label, blob = m.group(1), []
            continue
        if blob is None:
            continue
        if line.startswith(END_HEX):
            try:
                snaps.append(decode(bytes.fromhex("".join(blob)), label))
            except ValueError as exc:
                print(f"  warning: could not decode snapshot {label}: {exc}", file=sys.stderr)
            blob = None
            continue
        if HEX_LINE.match(line):
            blob.append(line)
    return snaps


def decode(buf, label):
    if len(buf) < 64:
        raise ValueError("shorter than a header")
    (magic, version, seq, node_count, edge_count, node_off, edge_off,
     adj_off, _res, total_frames, free_frames, ticks) = struct.unpack_from(
        "<IIQIIIIIIQQQ", buf, 0)
    if magic != INSPECT_MAGIC:
        raise ValueError(f"bad magic {magic:#x}")

    # Record sizes come from the offsets rather than from a hard-coded layout,
    # so a change on the kernel side shows up as a clean failure here instead
    # of silently misreading every field.
    if node_count == 0 or edge_count == 0:
        raise ValueError("empty snapshot")
    node_size = (adj_off - node_off) // node_count
    adj_size = (edge_off - adj_off) // node_count
    edge_size = (len(buf) - edge_off) // edge_count
    if node_size < 32 or edge_size < 40 or adj_size != 8:
        raise ValueError(
            f"unexpected record sizes: node {node_size}, adjacency {adj_size}, edge {edge_size}")

    snap = Snapshot(label, seq, node_count, edge_count, free_frames, total_frames, ticks)

    for i in range(node_count):
        at = node_off + i * node_size
        nid, kind, flags = struct.unpack_from("<QBB", buf, at)
        a, b = struct.unpack_from("<QQ", buf, at + 16)
        k, idx, gen = node_id(nid)
        snap.nodes[(k, idx, gen)] = Node(k, idx, gen, "dying" if flags & 1 else "-",
                                         describe_node(k, a, b))

    for i in range(edge_count):
        at = edge_off + i * edge_size
        src_raw, dst_raw, kind, flags = struct.unpack_from("<QQBB", buf, at)
        a, b = struct.unpack_from("<QQ", buf, at + 24)
        name = EDGE_KINDS[kind] if kind < len(EDGE_KINDS) else f"?{kind}"
        src, dst = node_id(src_raw), node_id(dst_raw)
        e = Edge(name, src, dst, describe_edge(name, a, b), bool(flags & EDGE_FLAG_VIRTUAL))
        snap.edges.append(e)
        if name == "Holds":
            snap.handles.setdefault(src, {})[b] = (dst, a)

    # The adjacency table is the reason the format exists; verify it agrees
    # with the edges rather than trusting it.
    seen = 0
    for i in range(node_count):
        first, count = struct.unpack_from("<II", buf, adj_off + i * adj_size)
        if first != seen:
            raise ValueError(f"adjacency entry {i} starts at {first}, expected {seen}")
        seen += count
    if seen != edge_count:
        raise ValueError(f"adjacency covers {seen} edges, header says {edge_count}")
    return snap


def describe_node(kind, a, b):
    if kind == "Root":
        return f"ticks={a} free={b}"
    if kind == "Cpu":
        return "running=" + ("-" if a == 0 else "%s#%d.%d" % node_id(a))
    if kind == "Process":
        return f"handles={a}"
    if kind == "Thread":
        states = ["inert", "ready", "running", "blocked", "dying"]
        return f"{states[a] if a < len(states) else a} cr3={b:#x}"
    if kind == "AddressSpace":
        return f"pml4={a:#x} mappings={b}"
    if kind == "MemoryObject":
        flags = b >> 32
        tag = "device" if flags & 1 else ("pinned" if flags & 2 else "ram")
        return f"{a:#x} {b & 0xFFFFFFFF}pg {tag}"
    if kind == "Device":
        return f"io={a:#x}"
    return ""


def describe_edge(kind, a, b):
    if kind == "Holds":
        return f"rights={rights_str(a)} slot={b}"
    if kind == "Maps":
        return f"{a:#x} {b & 0xFFFFFFFF}pg prot={b >> 32:#x}"
    if kind == "Named":
        raw = (a.to_bytes(8, "little") + b.to_bytes(8, "little")).rstrip(b"\0")
        return repr(raw.decode("utf-8", "replace"))
    if kind == "Waiting":
        return "send" if a == 0 else "recv"
    return ""


# ----------------------------------------------------------------- checks ---

def check(snap):
    """Verify offline what the kernel's checker verifies inside. Disagreement
    means one of the two is wrong, which is the point of having both."""
    problems = []
    if len(snap.nodes) != snap.declared_nodes:
        problems.append(f"header says {snap.declared_nodes} nodes, found {len(snap.nodes)}")
    if len(snap.edges) != snap.declared_edges:
        problems.append(f"header says {snap.declared_edges} edges, found {len(snap.edges)}")

    for e in snap.edges:
        if e.src not in snap.nodes:
            problems.append(f"{e.kind} edge from missing node {e.src}")
        if e.dst not in snap.nodes:
            problems.append(f"{e.kind} edge to missing node {e.dst}")

    owners = {}
    for e in snap.edges:
        if e.kind != "Owns":
            continue
        if e.dst in owners:
            problems.append(f"{e.dst} has more than one owner")
        owners[e.dst] = e.src

    roots = [k for k in snap.nodes if k[0] == "Root"]
    if len(roots) != 1:
        problems.append(f"expected exactly one Root, found {len(roots)}")
    for key, node in snap.nodes.items():
        if key[0] == "Root" or node.flags == "dying":
            continue
        if key not in owners:
            problems.append(f"{node.label} has no owner")
            continue
        seen, cur = set(), key
        while cur in owners:
            if cur in seen:
                problems.append(f"ownership cycle at {node.label}")
                break
            seen.add(cur)
            cur = owners[cur]
        else:
            if cur[0] != "Root":
                problems.append(f"{node.label} does not reach the root")

    # A thread is queued, or waiting, or neither. Never both.
    for key in snap.nodes:
        if key[0] != "Thread":
            continue
        ready = any(e.kind == "Ready" and e.dst == key for e in snap.edges)
        waiting = any(e.kind == "Waiting" and e.src == key for e in snap.edges)
        if ready and waiting:
            problems.append(f"{key} is both queued and waiting")
    return problems


# ------------------------------------------------------------ reachability ---

def could_ever_reach(snap, subject, target):
    """The take-grant safety question, from 1977: could this process *ever*
    obtain a capability to that object?

    Current authority is one edge. Potential authority is the closure of the
    ways a capability can move in this system, and this is that closure:

      - what a process already holds;
      - anything named, if it holds the root with `lookup`;
      - anything a process it can receive from could obtain, when that process
        can also grant over the same endpoint;
      - anything its parent could obtain, if the parent may grant into it.

    A conventional kernel cannot be asked this, because there is no structure
    that describes how authority moves.
    """
    named = {e.dst for e in snap.edges if e.kind == "Named"}
    reach = {}
    for p in (k for k in snap.nodes if k[0] == "Process"):
        held = {t for t, _ in snap.handles.get(p, {}).values()}
        if any(t[0] == "Root" and (bits & R_LOOKUP) for t, bits in snap.handles.get(p, {}).values()):
            held |= named
        reach[p] = held

    # Who can pass a capability to whom.
    channels = []  # (from, to)
    for p, slots in snap.handles.items():
        senders = {t for t, bits in slots.values()
                   if t[0] == "Endpoint" and (bits & R_SEND) and (bits & R_GRANT)}
        for q, qslots in snap.handles.items():
            if q == p:
                continue
            if senders & {t for t, bits in qslots.values()
                          if t[0] == "Endpoint" and (bits & R_RECV)}:
                channels.append((p, q))
    for p, slots in snap.handles.items():
        for t, bits in slots.values():
            if t[0] == "Process" and (bits & R_GRANT):
                channels.append((p, t))

    changed = True
    while changed:
        changed = False
        for src, dst in channels:
            if src in reach and dst in reach and not reach[src] <= reach[dst]:
                reach[dst] |= reach[src]
                changed = True
    return target in reach.get(subject, set()), reach


# ------------------------------------------------------------------- draw ---

NODE_STYLE = {
    "Root": ("doubleoctagon", "#86c232"), "Cpu": ("box3d", "#4aa3df"),
    "Process": ("box", "#d9a441"), "Thread": ("ellipse", "#e0842a"),
    "AddressSpace": ("folder", "#9b7fd4"), "MemoryObject": ("note", "#7ec8a0"),
    "Endpoint": ("diamond", "#d96ba0"), "Device": ("component", "#c0c0c0"),
}
EDGE_STYLE = {
    "Owns": ("solid", "#333333", "2.0"), "Holds": ("dashed", "#d9534f", "1.2"),
    "InSpace": ("solid", "#9b7fd4", "1.2"), "Maps": ("solid", "#7ec8a0", "1.2"),
    "Ready": ("bold", "#4aa3df", "1.6"), "Waiting": ("dotted", "#d96ba0", "1.6"),
    "Named": ("dotted", "#888888", "0.8"), "Running": ("bold", "#e0842a", "2.0"),
}


def to_dot(snap, hide=()):
    out = [
        "digraph bramble {",
        '  graph [bgcolor="#101410", fontname="DejaVu Sans", fontcolor="#c8d0c8",'
        f' label="bramble kernel state  |  seq {snap.seq}  |  {len(snap.nodes)} nodes,'
        f' {len(snap.edges)} edges  |  {snap.free_frames} of {snap.total_frames} frames free",'
        " labelloc=t, rankdir=LR, splines=true];",
        '  node [fontname="DejaVu Sans Mono", fontsize=10, style="filled",'
        ' fontcolor="#101410", penwidth=0];',
        '  edge [fontname="DejaVu Sans Mono", fontsize=8, fontcolor="#a0a8a0"];',
    ]
    for key, n in sorted(snap.nodes.items()):
        shape, colour = NODE_STYLE.get(n.kind, ("box", "#cccccc"))
        detail = n.attrs.replace('"', "'")
        label = n.label if not detail else f"{n.label}\\n{detail}"
        extra = ' penwidth=3 color="#e05a4a"' if n.flags == "dying" else ""
        out.append(f'  "{n.label}" [shape={shape}, fillcolor="{colour}", label="{label}"{extra}];')
    for e in snap.edges:
        if e.kind in hide:
            continue
        style, colour, width = EDGE_STYLE.get(e.kind, ("solid", "#999999", "1.0"))
        src = "%s#%d.%d" % e.src
        dst = "%s#%d.%d" % e.dst
        label = e.kind if not e.attrs else f"{e.kind} {e.attrs}".replace('"', "'")
        if e.virtual:
            label += " (virtual)"
            style = "dashed"
        out.append(f'  "{src}" -> "{dst}" [style={style}, color="{colour}",'
                   f' penwidth={width}, label="{label}"];')
    out.append("}")
    return "\n".join(out)


def diff(a, b):
    an, bn = set(a.nodes), set(b.nodes)
    ae = {e.key for e in a.edges}
    be = {e.key for e in b.edges}
    lines = []
    for k in sorted(bn - an):
        lines.append(f"  + node {k[0]}#{k[1]}.{k[2]}")
    for k in sorted(an - bn):
        lines.append(f"  - node {k[0]}#{k[1]}.{k[2]}")
    for k in sorted(be - ae):
        lines.append(f"  + edge {k[0]} {k[1][0]}#{k[1][1]}.{k[1][2]} -> {k[2][0]}#{k[2][1]}.{k[2][2]} {k[3]}")
    for k in sorted(ae - be):
        lines.append(f"  - edge {k[0]} {k[1][0]}#{k[1][1]}.{k[1][2]} -> {k[2][0]}#{k[2][1]}.{k[2][2]} {k[3]}")
    return lines


def find(snap, label):
    for key in snap.nodes:
        if "%s#%d.%d" % key == label or f"{key[0]}#{key[1]}" == label:
            return key
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("log")
    ap.add_argument("--dot")
    ap.add_argument("--png")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--diff", action="store_true",
                    help="diff the last two snapshots in the log")
    ap.add_argument("--reach", nargs=2, metavar=("PROCESS", "OBJECT"),
                    help="could this process ever obtain a capability to that object?")
    ap.add_argument("--index", type=int, default=-1)
    ap.add_argument("--hide", default="Named",
                    help="comma-separated edge kinds to leave out of the picture")
    ap.add_argument("--prefer-text", action="store_true",
                    help="use the kernel's own dump even when a snapshot exists")
    args = ap.parse_args()

    with open(args.log, "rb") as f:
        text = f.read().decode("utf-8", "replace")

    binary = parse_hex(text)
    textual = parse_text(text)
    snaps = textual if (args.prefer_text or not binary) else binary
    if not snaps:
        sys.exit("no kernel state found in the log")
    source = "text dump" if snaps is textual else "inspect snapshot"
    snap = snaps[args.index]
    print(f"read {len(snaps)} {source}(s); using '{snap.label}' at seq {snap.seq}: "
          f"{len(snap.nodes)} nodes, {len(snap.edges)} edges")

    failed = False
    if args.check:
        problems = check(snap)
        for p in problems:
            print(f"  VIOLATION: {p}")
        if problems:
            failed = True
        else:
            print("  offline checker agrees with the kernel: all invariants hold")

    if args.diff:
        if len(snaps) < 2:
            print("  only one snapshot in the log; nothing to diff")
        else:
            lines = diff(snaps[-2], snaps[-1])
            print(f"  between '{snaps[-2].label}' (seq {snaps[-2].seq}) and "
                  f"'{snaps[-1].label}' (seq {snaps[-1].seq}):")
            print("\n".join(lines) if lines else "    (identical)")

    if args.reach:
        subject = find(snap, args.reach[0])
        target = find(snap, args.reach[1])
        if not subject or not target:
            sys.exit(f"could not find {args.reach[0]} or {args.reach[1]} in the snapshot")
        answer, reach = could_ever_reach(snap, subject, target)
        verb = "COULD" if answer else "could never"
        print(f"  {'%s#%d.%d' % subject} {verb} obtain a capability to {'%s#%d.%d' % target}")
        held = sorted("%s#%d.%d" % t for t in reach.get(subject, ()))
        print(f"    its potential authority covers {len(held)} objects: {', '.join(held)}")

    hide = tuple(x for x in args.hide.split(",") if x)
    if args.dot or args.png:
        dot = to_dot(snap, hide)
        if args.dot:
            with open(args.dot, "w") as f:
                f.write(dot)
            print(f"wrote {args.dot}")
        if args.png:
            if not shutil.which("dot"):
                sys.exit("graphviz 'dot' is not installed")
            subprocess.run(["dot", "-Tpng", "-o", args.png], input=dot.encode(), check=True)
            print(f"wrote {args.png}")

    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
