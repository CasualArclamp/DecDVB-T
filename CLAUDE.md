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
cargo run --release -p decdvbt-cli -- modulate --cn 25 --profile urban  # a DVB-T test signal file
cargo run --release -p decdvbt-cli -- sound captures/c26_514.000MHz_10Msps.cs8  # lock, offsets, paths
cargo run --release -p decdvbt-cli -- decode captures/c23_490.000MHz_10Msps_amp.cs8 -o out/c23.ts  # a transport stream, the services, RS and CPU stats
cargo run --release -p decdvbt-cli -- capture C23 --amp --lna 32 --vga 8 --record x.cs8  # the RF amp must be on for DVB-T
cargo run --release -p decdvbt-rx --example diag -- x.cs8   # per-symbol equaliser diagnostics (see its header)
cargo run --release -p decdvbt-gui                               # the GUI
cargo run --release -p decdvbt-gui -- captures/c23_490.000MHz_10Msps_amp.cs8 --decode --screenshot out/x.png
cargo run -p decdvbt-gui --example make_icon                      # redraws apps/decdvbt-gui/assets/icon.ico and icon-256.png (preview in out/)
cargo run --release -p decdvbt-gui --example retune_soak -- C23 C26 C28 --rounds 20   # HackRF: tune between channels like the GUI, time each lock
cargo run --release -p decdvbt-rx --example splice -- A.cs8 B.cs8 24            # offline: the old channel's samples then the new one's; ACQSCAN=1 for first-attempt acquisition
# the GUI's extras (screenshots, demos, tests): --bottom scopes|channel|none, --window ts|epg,
# --service ID (select), --play ID, --http 127.0.0.1:8001, --udp 127.0.0.1:1234, --record
```

Captures live in `captures/` and are named by their tuning
(`c26_514.000MHz_10Msps_….cs8`); the GUI and CLI read the centre and rate from
the name. They are not committed.

Rust is installed through rustup (stable MSVC). In Git Bash, `~/.cargo/bin`
may need adding to PATH. The player needs `ffmpeg` at run time: on the PATH,
beside the exe, or installed by `winget install Gyan.FFmpeg` (found in winget's
package folder even before the PATH is refreshed).

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
