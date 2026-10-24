// made by CasualArclamp
//! Print an MHEG-5 object's tree with tag names:
//! `cargo run -p decdvbt-mheg --example mhegdump -- out/rb4416/a`.

use decdvbt_mheg::asn1::{Node, parse};
use decdvbt_mheg::tags::tag_name;

fn show(n: &Node, depth: usize) {
    let pad = "  ".repeat(depth);
    match n {
        Node::Tagged(t, a) => {
            let flat = a
                .iter()
                .all(|x| !matches!(x, Node::Tagged(..) | Node::Seq(_)));
            if flat {
                let v: Vec<String> = a.iter().map(leaf).collect();
                println!("{pad}:{} {}", tag_name(*t), v.join(" "));
            } else {
                println!("{pad}:{}", tag_name(*t));
                for x in a {
                    show(x, depth + 1);
                }
            }
        }
        Node::Seq(a) => {
            let flat = a
                .iter()
                .all(|x| !matches!(x, Node::Tagged(..) | Node::Seq(_)));
            if flat {
                let v: Vec<String> = a.iter().map(leaf).collect();
                println!("{pad}( {} )", v.join(" "));
            } else {
                println!("{pad}(");
                for x in a {
                    show(x, depth + 1);
                }
                println!("{pad})");
            }
        }
        x => println!("{pad}{}", leaf(x)),
    }
}

fn leaf(n: &Node) -> String {
    match n {
        Node::Bool(b) => b.to_string(),
        Node::Int(i) => i.to_string(),
        Node::Enum(i) => format!("e{i}"),
        Node::Str(s) => format!("{:?}", String::from_utf8_lossy(s)),
        Node::Null => "null".into(),
        _ => "...".into(),
    }
}

fn main() {
    let p = std::env::args().nth(1).expect("a file");
    let b = std::fs::read(&p).expect("read");
    match parse(&b) {
        Ok(n) => show(&n, 0),
        Err(e) => eprintln!("{e}"),
    }
}
