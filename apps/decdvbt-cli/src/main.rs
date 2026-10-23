// made by CasualArclamp
//! DecDVB-T command line.
//!
//! M0 provides `channels` (the UHF channel plan) and `channel` (one channel's
//! frequency). Recording, `bench` and decoding arrive with later milestones.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use decdvbt_core::channel::{parse_channel, uhf_centre_hz, uhf_channel_at, uk_channels};

#[derive(Parser)]
#[command(name = "decdvbt", version, about = "DVB-T / DVB-T2 receiver")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the UK UHF channels and their centre frequencies.
    Channels,
    /// One channel's centre frequency ("C34", "34"), or the channel holding a
    /// frequency in MHz ("578").
    Channel { which: String },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Channels => {
            for n in uk_channels() {
                println!("C{n:<3} {:>7.3} MHz", uhf_centre_hz(n).unwrap() / 1e6);
            }
        }
        Command::Channel { which } => {
            if let Some(n) = parse_channel(&which) {
                println!("C{n} {:.3} MHz", uhf_centre_hz(n).unwrap() / 1e6);
            } else if let Ok(mhz) = which.parse::<f64>() {
                match uhf_channel_at(mhz * 1e6) {
                    Some(n) => println!("C{n} {:.3} MHz", uhf_centre_hz(n).unwrap() / 1e6),
                    None => bail!("{mhz} MHz is not in a UHF channel (21–69)"),
                }
            } else {
                bail!("not a channel or a frequency: {which}");
            }
        }
    }
    Ok(())
}
