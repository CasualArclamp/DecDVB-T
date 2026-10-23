// made by CasualArclamp
//! A small seeded random generator (xoshiro256**), so a test signal can be
//! made again exactly: the same seed gives the same noise, fades and bursts.

pub struct Rng {
    s: [u64; 4],
}

fn splitmix64(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut x = seed;
        Rng {
            s: std::array::from_fn(|_| splitmix64(&mut x)),
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in (0, 1).
    pub fn uniform(&mut self) -> f64 {
        // 53 random bits, never exactly 0 or 1.
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Two independent standard normals (Box–Muller).
    pub fn gaussian_pair(&mut self) -> (f64, f64) {
        let (u1, u2) = (self.uniform(), self.uniform());
        let r = (-2.0 * u1.ln()).sqrt();
        let (s, c) = (std::f64::consts::TAU * u2).sin_cos();
        (r * c, r * s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_numbers() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        assert!((0..100).all(|_| a.next_u64() == b.next_u64()));
        assert_ne!(Rng::new(7).next_u64(), Rng::new(8).next_u64());
    }

    #[test]
    fn gaussians_have_unit_variance_and_zero_mean() {
        let mut r = Rng::new(1);
        let n = 200_000;
        let (mut sum, mut sq) = (0.0, 0.0);
        for _ in 0..n / 2 {
            let (a, b) = r.gaussian_pair();
            sum += a + b;
            sq += a * a + b * b;
        }
        assert!((sum / n as f64).abs() < 0.01);
        assert!((sq / n as f64 - 1.0).abs() < 0.01);
    }

    #[test]
    fn uniforms_stay_inside_zero_and_one() {
        let mut r = Rng::new(3);
        for _ in 0..100_000 {
            let u = r.uniform();
            assert!(u > 0.0 && u < 1.0);
        }
    }
}
