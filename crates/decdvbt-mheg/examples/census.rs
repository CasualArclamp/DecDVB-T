// made by CasualArclamp
//! Which classes and elementary actions the MHEG-5 objects in a folder use
//! (a dumped carousel): `cargo run -p decdvbt-mheg --example census -- out/rb`.

use std::collections::BTreeMap;

use decdvbt_mheg::asn1::{Node, parse};
use decdvbt_mheg::tags::tag_name;

fn walk(n: &Node, counts: &mut BTreeMap<String, usize>) {
    if let Node::Tagged(t, _) = n {
        *counts.entry(format!("{t:3} {}", tag_name(*t))).or_default() += 1;
    }
    for x in n.args() {
        walk(x, counts);
    }
}

fn main() {
    let dir = std::env::args().nth(1).expect("a folder");
    let mut counts = BTreeMap::new();
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            for e in std::fs::read_dir(&p).expect("read the folder") {
                stack.push(e.expect("a folder entry").path());
            }
            continue;
        }
        let b = std::fs::read(&p).expect("read a file");
        // An application or a scene starts [0] or [1], constructed.
        if b.first().is_some_and(|&c| c == 0xA0 || c == 0xA1) {
            match parse(&b) {
                Ok(n) => walk(&n, &mut counts),
                Err(e) => println!("{}: {e}", p.display()),
            }
        }
    }
    for (k, v) in counts {
        println!("{v:6} {k}");
    }
}
