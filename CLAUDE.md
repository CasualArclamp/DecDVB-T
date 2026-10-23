<!-- made by CasualArclamp -->
# DecDVB-T — working notes

Read [`docs/DESIGN.md`](docs/DESIGN.md) first. It holds the agreed scope, the
crate layout and the milestone plan. Update it when a decision changes.
[`docs/STATUS.md`](docs/STATUS.md) logs what is done.

## Commands

```bash
cargo check --workspace --all-targets   # fast feedback
cargo test --workspace                  # unit tests
cargo clippy --workspace --all-targets -- -D warnings
cargo run --release -p decdvbt-cli -- channels
cargo run --release -p decdvbt-cli -- scan                       # HackRF: which channels carry a mux
cargo run --release -p decdvbt-cli -- capture C26 --record x.cs8 # level, drops, CPU; optional IQ
cargo run --release -p decdvbt-gui                               # the GUI
cargo run --release -p decdvbt-gui -- captures/c26_514.000MHz_10Msps.cs8 --screenshot out/x.png
```

Captures live in `captures/` and are named by their tuning
(`c26_514.000MHz_10Msps_….cs8`); the GUI and CLI read the centre and rate from
the name. They are not committed.

Rust is installed through rustup (stable MSVC). In Git Bash, `~/.cargo/bin`
may need adding to PATH. The player needs `ffmpeg` on PATH at run time.

## House rules

- **Licence:** GPL-3.0-or-later. Algorithms are borrowed from GPL-3 code
  (gr-dtv, OnAir, Ron Economos's DVB-T2 work, DecSAT, DecDRM). Credit the
  source in a comment on the function or module that borrows from it.
- **Never commit** IQ captures, reference clones (`reference/`), or built exes.
- **The specs are the authority:** ETSI EN 300 744 (DVB-T), EN 302 755
  (DVB-T2), TS 102 831 (T2 implementation guidelines). Cite the clause or
  table number in a comment next to constants, tables and bit layouts.
- Rory has a strong DSP background and is newer to Rust. Explain Rust idioms
  in comments, not DSP basics.
- **CPU efficiency is the main requirement** (DESIGN.md §0). Weigh every
  change against it, and measure before and after on a capture.
- DSP is unusably slow unoptimised. Use `--release` for anything that depends
  on timing.
- Heavy test sweeps belong on CI, not on this machine.
