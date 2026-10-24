# made by CasualArclamp
# Freesat/Freeview Huffman tables (DVB text 0x1F) from MythTV's freesat_tables.cpp to Rust:
#   python -I tools/gen_huffman.py path/to/freesat_tables.cpp crates/decdvbt-ts/src/huffman_tables.rs
import re
import sys

src = open(sys.argv[1], encoding="utf-8", errors="replace").read()
out_path = sys.argv[2]


def block(name):
    i = src.index(name)
    a = src.index("{", i)
    b = src.index("};", a)
    return src[a + 1 : b]


def entries(name):
    rows = re.findall(r"\{\s*(0x[0-9a-fA-F]+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\}", block(name))
    return [(int(v, 16), int(bits), int(nxt)) for v, bits, nxt in rows]


def index(name):
    body = re.sub(r"/\*.*?\*/", "", block(name), flags=re.S)
    return [int(x) for x in re.findall(r"\d+", body)]


t1, t2 = entries("fsat_table_1"), entries("fsat_table_2")
i1, i2 = index("fsat_index_1"), index("fsat_index_2")
assert len(i1) == 129 and len(i2) == 129, (len(i1), len(i2))
assert i1[-1] == len(t1) and i2[-1] == len(t2), (i1[-1], len(t1), i2[-1], len(t2))
for v, bits, nxt in t1 + t2:
    assert 1 <= bits <= 32 and nxt < 256 and v < 2**32

lines = [
    "// made by CasualArclamp",
    "//! The Freesat/Freeview Huffman tables for DVB text with the 0x1F selector",
    "//! (table 1 and table 2): for each previous character (0 for the start, an",
    "//! ASCII code otherwise) the codes that may follow it, MSB-aligned in a u32,",
    "//! their length in bits and the character they stand for (0 stops, 1",
    "//! escapes to a raw byte).",
    "//!",
    "//! Generated from MythTV's `libs/libmythtv/mpeg/freesat_tables.cpp`",
    "//! (GPL-2.0-or-later, the MythTV project; the tables are the broadcasters')",
    "//! by `tools/gen_huffman.py`. Do not edit by hand.",
    "",
    "/// A code: its bits MSB-aligned, how many, the character it decodes to.",
    "pub(crate) type Code = (u32, u8, u8);",
    "",
]


def emit(name, table, idx):
    lines.append(f"pub(crate) static {name}_CODES: [Code; {len(table)}] = [")
    for v, bits, nxt in table:
        lines.append(f"    (0x{v:08x}, {bits}, {nxt}),")
    lines.append("];")
    lines.append("")
    lines.append(f"/// Where each previous character's codes start in `{name}_CODES`.")
    lines.append(f"pub(crate) static {name}_INDEX: [u16; 129] = [")
    for k in range(0, 129, 12):
        lines.append("    " + " ".join(f"{x}," for x in idx[k : k + 12]))
    lines.append("];")
    lines.append("")


emit("TABLE_1", t1, i1)
emit("TABLE_2", t2, i2)
open(out_path, "w", encoding="utf-8", newline="\n").write("\n".join(lines))
print(len(t1), len(t2))
