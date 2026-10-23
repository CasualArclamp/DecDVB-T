// made by CasualArclamp
//! Loopback: modulate, then undo every stage with an ideal receiver (no
//! channel) and compare the transport packets.
//!
//! This proves the stages are inverses of one another, the pilots and TPS
//! are where and what they should be by the tables, and the whole chain is
//! consistent. It does not prove the tables match a broadcaster's: that is
//! for the real captures (M3).

use decdvbt_core::Iq;
use decdvbt_core::dvbt::Mode;
use decdvbt_fec::conv;
use decdvbt_fec::rs::ReedSolomon;
use decdvbt_mod::Modulator;
use decdvbt_mod::inner::conv_rate;
use decdvbt_mod::outer::{ConvInterleaver, Prbs, RS_LEN, SYNC_INVERTED};
use decdvbt_mod::ts::{TS_LEN, TS_SYNC, TestStream, TsSource};
use decdvbt_ofdm::interleave::{BLOCK, bit_deinterleave, symbol_deinterleave, symbol_permutation};
use decdvbt_ofdm::mapping::Constellation;
use decdvbt_ofdm::pilots::{Role, SYMBOLS_PER_FRAME, SymbolPlan, pilot_value, prbs, tps_carriers};
use decdvbt_ofdm::tps;
use num_complex::Complex32;
use rustfft::FftPlanner;

struct Run {
    /// The transport packets recovered, from the first fully valid one.
    packets: Vec<[u8; TS_LEN]>,
    /// The first packet's index in the stream.
    first_index: usize,
    /// RS blocks that needed correcting (there should be none).
    rs_corrections: usize,
    tps: Vec<tps::TpsFrame>,
}

fn loopback(mode: Mode, symbols: usize, cell_id: u16) -> Run {
    let mut m = Modulator::new(mode, Some(cell_id), Box::new(TestStream::default())).unwrap();
    let n = mode.fft.n();
    let guard = n / mode.guard.divisor();
    let scale = m.carrier_scale();
    let k_total = mode.fft.active_carriers();
    let kmid = (k_total - 1) / 2;
    let plan = SymbolPlan::new(mode.fft);
    let h = symbol_permutation(mode.fft);
    let wk = prbs(k_total);
    let constellation =
        Constellation::new(mode.modulation, decdvbt_core::dvbt::Alpha::NonHierarchical);
    let fft = FftPlanner::new().plan_fft_forward(n);
    let v = mode.modulation.bits();

    let mut coded_bits: Vec<u8> = Vec::new();
    let mut tps_prev: Vec<Complex32> = vec![Complex32::new(0.0, 0.0); tps_carriers(mode.fft).len()];
    let mut tps_bits = [0u8; tps::TPS_BITS];
    let mut tps_found = Vec::new();

    let mut sym = Vec::new();
    for i in 0..symbols {
        let l = i % SYMBOLS_PER_FRAME;
        m.next_symbol(&mut sym);
        assert_eq!(sym.len(), n + guard);
        // The guard interval is a copy of the symbol's end.
        for j in 0..guard {
            assert_eq!(sym[j], sym[n + j], "symbol {i} guard sample {j}");
        }
        let mut x: Vec<Iq> = sym[guard..].to_vec();
        fft.process(&mut x);
        let carrier = |k: usize| {
            let bin = (k as isize - kmid as isize).rem_euclid(n as isize) as usize;
            x[bin] / (n as f32 * scale)
        };

        let mut words = Vec::new();
        let (mut tps_i, mut votes) = (0, 0i32);
        for (k, role) in plan.roles(l).iter().enumerate() {
            let c = carrier(k);
            match role {
                Role::Data => words.push(constellation.nearest(c)),
                Role::Pilot => {
                    let want = pilot_value(wk[k]);
                    assert!(
                        (c.re - want).abs() < 1e-3 && c.im.abs() < 1e-3,
                        "pilot {k} symbol {i}"
                    );
                }
                Role::Tps => {
                    if l == 0 {
                        // The reference: ±1 from the PRBS.
                        let want = 2.0 * (0.5 - f32::from(wk[k]));
                        assert!((c.re - want).abs() < 1e-3, "TPS ref {k}");
                    } else {
                        // DBPSK: a flip is a 1.
                        votes += if (c * tps_prev[tps_i].conj()).re < 0.0 {
                            1
                        } else {
                            -1
                        };
                    }
                    tps_prev[tps_i] = c;
                    tps_i += 1;
                }
            }
        }
        if l > 0 {
            tps_bits[l] = u8::from(votes > 0);
            // Every carrier agreed.
            assert_eq!(votes.unsigned_abs() as usize, tps_carriers(mode.fft).len());
        }
        if l == SYMBOLS_PER_FRAME - 1 {
            tps_found.push(tps::parse(&tps_bits).expect("TPS parses"));
        }

        // Undo the symbol interleaver and the bit interleaver, word by word.
        assert_eq!(words.len(), mode.fft.data_carriers());
        let mut y = vec![0u8; words.len()];
        symbol_deinterleave(&h, l, &words, &mut y);
        let mut z = vec![0u8; words.len()];
        let (src, dst) = (y.as_chunks::<BLOCK>().0, z.as_chunks_mut::<BLOCK>().0);
        for (a, b) in src.iter().zip(dst) {
            bit_deinterleave(mode.modulation, a, b);
        }
        for w in z {
            for b in (0..v).rev() {
                coded_bits.push((w >> b) & 1);
            }
        }
    }

    // Viterbi on ±1 (a 0 bit is positive).
    let soft: Vec<f32> = coded_bits
        .iter()
        .map(|&b| 1.0 - 2.0 * f32::from(b))
        .collect();
    let info = conv::decode_block(conv_rate(mode.rate_hp), &soft);
    let bytes: Vec<u8> = info
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b))
        .collect();

    // The byte deinterleaver, then 204-byte blocks: block b is packet b − 11.
    let mut d = ConvInterleaver::new(true);
    let plain: Vec<u8> = bytes.iter().map(|&b| d.push(b)).collect();
    let rs = ReedSolomon::dvb();
    let delay_blocks = 11;
    let blocks: Vec<&[u8; RS_LEN]> = plain.as_chunks::<RS_LEN>().0.iter().collect();
    let mut packets = Vec::new();
    let mut rs_corrections = 0;
    let mut prbs_rx = Prbs::new();
    // The tail (the Viterbi's last decisions) is not trusted.
    for (b, block) in blocks
        .iter()
        .enumerate()
        .take(blocks.len().saturating_sub(3))
    {
        if b < delay_blocks {
            continue;
        }
        let mut blk = block.to_vec();
        if rs.decode(&mut blk) != Ok(0) {
            rs_corrections += 1;
        }
        let p = b - delay_blocks;
        if p % 8 == 0 {
            assert_eq!(blk[0], SYNC_INVERTED, "packet {p} should open a group");
            blk[0] = TS_SYNC;
            prbs_rx.reset();
        } else {
            prbs_rx.bits(8);
        }
        for byte in &mut blk[1..TS_LEN] {
            *byte ^= prbs_rx.bits(8);
        }
        let mut pkt = [0u8; TS_LEN];
        pkt.copy_from_slice(&blk[..TS_LEN]);
        packets.push(pkt);
    }
    Run {
        packets,
        first_index: 0,
        rs_corrections,
        tps: tps_found,
    }
}

fn check(mode_text: &str, symbols: usize) {
    let mode = Mode::parse(mode_text).unwrap();
    let run = loopback(mode, symbols, 0x1234);
    assert_eq!(run.rs_corrections, 0, "{mode_text}: RS had to correct");
    assert!(
        run.packets.len() >= 40,
        "{mode_text}: only {} packets",
        run.packets.len()
    );
    // Every packet is the one the source made, in order.
    let mut src = TestStream::default();
    let mut want = [0u8; TS_LEN];
    for _ in 0..run.first_index {
        src.next_packet(&mut want);
    }
    for (i, got) in run.packets.iter().enumerate() {
        src.next_packet(&mut want);
        assert_eq!(got, &want, "{mode_text}: packet {i}");
    }
    // The TPS names the mode, frame by frame.
    assert!(!run.tps.is_empty());
    for (f, t) in run.tps.iter().enumerate() {
        assert_eq!(t.frame as usize, f % 4);
        assert_eq!(
            (t.modulation, t.rate_hp, t.guard, t.fft),
            (mode.modulation, mode.rate_hp, mode.guard, mode.fft)
        );
        assert_eq!(t.length, 31);
        assert_eq!(t.cell_id_byte, if f % 2 == 0 { 0x12 } else { 0x34 });
    }
}

#[test]
fn qpsk_half_2k_quarter_guard() {
    check("qpsk-1/2-2k-1/4", 2 * SYMBOLS_PER_FRAME);
}

#[test]
fn qam16_three_quarters_2k_eighth_guard() {
    check("16qam-3/4-2k-1/8", 2 * SYMBOLS_PER_FRAME);
}

#[test]
fn qam64_seven_eighths_2k_sixteenth_guard() {
    check("64qam-7/8-2k-1/16", 2 * SYMBOLS_PER_FRAME);
}

#[test]
fn qam64_two_thirds_8k_the_uk_mode() {
    check("64qam-2/3-8k-1/32", SYMBOLS_PER_FRAME);
}

#[test]
fn qam16_five_sixths_8k_quarter_guard() {
    check("16qam-5/6-8k-1/4", SYMBOLS_PER_FRAME);
}
