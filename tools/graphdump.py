#!/usr/bin/env python3
"""Turn Bramble's serial graph dump into a picture, and check it offline.

The kernel emits its entire state as text between two markers. This reads that
block out of a serial log and either renders it with Graphviz or re-verifies
the structural invariants from outside the kernel, which is the point of having
one uniform representation: the same checker logic runs in two places.

    python3 tools/graphdump.py build/serial.log --dot build/graph.dot
    python3 tools/graphdump.py build/serial.log --png build/graph.png
    python3 tools/graphdump.py build/serial.log --check
"""
import argparse
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass, field

BEGIN = re.compile(r"--- graph begin seq=(\d+) nodes=(\d+) edges=(\d+) ---")
END = "--- graph end ---"
NODE = re.compile(r"^node (\w+)#(\d+)\.(\d+) (\S+)\s*(.*)$")
EDGE = re.compile(r"^edge (\w+) (\w+)#(\d+)\.(\d+) -> (\w+)#(\d+)\.(\d+)\s*(.*)$")

NODE_STYLE = {
    "Root": ("doubleoctagon", "#86c232"),
    "Cpu": ("box3d", "#4aa3df"),
    "Process": ("box", "#d9a441"),
    "Thread": ("ellipse", "#e0842a"),
    "AddressSpace": ("folder", "#9b7fd4"),
    "MemoryObject": ("note", "#7ec8a0"),
    "Endpoint": ("diamond", "#d96ba0"),
    "Device": ("component", "#c0c0c0"),
}

EDGE_STYLE = {
    "Owns": ("solid", "#333333", "2.0"),
    "Holds": ("dashed", "#d9534f", "1.2"),
    "InSpace": ("solid", "#9b7fd4", "1.2"),
    "Maps": ("solid", "#7ec8a0", "1.2"),
    "Ready": ("bold", "#4aa3df", "1.6"),
    "Waiting": ("dotted", "#d96ba0", "1.4"),
    "Named": ("dotted", "#888888", "0.8"),
}


@dataclass
class Node:
    kind: str
    idx: int
    gen: int
    flags: str
    attrs: str

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
    attrs: str


@dataclass
class Snapshot:
    seq: int = 0
    declared_nodes: int = 0
    declared_edges: int = 0
    nodes: dict = field(default_factory=dict)
    edges: list = field(default_factory=list)


def parse(text):
    """Return every graph block in the log, oldest first."""
    snaps, cur = [], None
    for line in text.splitlines():
        line = line.strip()
        m = BEGIN.search(line)
        if m:
            cur = Snapshot(int(m.group(1)), int(m.group(2)), int(m.group(3)))
            continue
        if cur is None:
            continue
        if line.startswith(END):
            snaps.append(cur)
            cur = None
            continue
        m = NODE.match(line)
        if m:
            n = Node(m.group(1), int(m.group(2)), int(m.group(3)), m.group(4), m.group(5))
            cur.nodes[n.key] = n
            continue
        m = EDGE.match(line)
        if m:
            cur.edges.append(
                Edge(
                    m.group(1),
                    (m.group(2), int(m.group(3)), int(m.group(4))),
                    (m.group(5), int(m.group(6)), int(m.group(7))),
                    m.group(8),
                )
            )
    return snaps


def check(snap):
    """Re-verify offline what the kernel's checker verifies inside. Any
    disagreement means one of the two is wrong, which is worth knowing."""
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

    # I2: Owns is a tree rooted at the one Root.
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
    return problems


def to_dot(snap):
    out = [
        "digraph bramble {",
        '  graph [bgcolor="#101410", fontname="DejaVu Sans", fontcolor="#c8d0c8",'
        f' label="bramble kernel state  |  seq {snap.seq}  |  {len(snap.nodes)} nodes,'
        f' {len(snap.edges)} edges", labelloc=t, rankdir=LR, splines=true];',
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
        style, colour, width = EDGE_STYLE.get(e.kind, ("solid", "#999999", "1.0"))
        src = f"{e.src[0]}#{e.src[1]}.{e.src[2]}"
        dst = f"{e.dst[0]}#{e.dst[1]}.{e.dst[2]}"
        label = e.kind if not e.attrs else f"{e.kind} {e.attrs}".replace('"', "'")
        out.append(
            f'  "{src}" -> "{dst}" [style={style}, color="{colour}",'
            f' penwidth={width}, label="{label}"];'
        )
    out.append("}")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("log", help="serial log containing at least one graph dump")
    ap.add_argument("--dot", help="write Graphviz source here")
    ap.add_argument("--png", help="render a picture here (needs graphviz)")
    ap.add_argument("--check", action="store_true", help="verify invariants offline")
    ap.add_argument("--index", type=int, default=-1, help="which dump to use (default: the last)")
    args = ap.parse_args()

    with open(args.log, "rb") as f:
        text = f.read().decode("utf-8", "replace")
    snaps = parse(text)
    if not snaps:
        sys.exit("no graph dump found in the log")
    snap = snaps[args.index]
    print(f"parsed {len(snaps)} dump(s); using seq {snap.seq}: "
          f"{len(snap.nodes)} nodes, {len(snap.edges)} edges")

    if args.check:
        problems = check(snap)
        for p in problems:
            print(f"  VIOLATION: {p}")
        if problems:
            sys.exit(1)
        print("  offline checker agrees with the kernel: all invariants hold")

    dot = to_dot(snap)
    if args.dot:
        with open(args.dot, "w") as f:
            f.write(dot)
        print(f"wrote {args.dot}")
    if args.png:
        if not shutil.which("dot"):
            sys.exit("graphviz 'dot' is not installed")
        subprocess.run(["dot", "-Tpng", "-o", args.png], input=dot.encode(), check=True)
        print(f"wrote {args.png}")


if __name__ == "__main__":
    main()
