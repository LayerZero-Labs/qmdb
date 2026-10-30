//! Zipf distribution sampler using the Rejection-Inversion algorithm.
//!
//! Reference: Hormann & Derflinger (1996), "Rejection-Inversion to Generate
//! Variates from Monotone Discrete Distributions".
//!
//! This implementation matches jonhoo's `zipf` crate (the Rust standard reference).
//!
//! Exponent guidelines:
//!   0.99 → YCSB default (highly skewed)
//!   1.5  → extremely skewed

use qmdb::test_helper::RandSrc;

pub struct ZipfSampler {
    n: u64,
    exponent: f64,
    h_integral_x1: f64,
    h_integral_n: f64,
    s: f64,
}

impl ZipfSampler {
    /// Create a new sampler for n elements with the given Zipf exponent (> 0, != 1.0).
    pub fn new(n: u64, exponent: f64) -> Self {
        assert!(n >= 1, "n must be >= 1");
        assert!(exponent > 0.0, "exponent must be > 0");
        assert!(
            (exponent - 1.0).abs() > 1e-10,
            "exponent must not be exactly 1.0; use 0.99 or 1.01"
        );

        // Matches jonhoo/zipf crate exactly
        let h_integral_n = h_integral(n as f64 + 0.5, exponent);
        let h_integral_x1 = h_integral(1.5, exponent) - 1.0;
        let s = 2.0 - h_integral_inverse(
            h_integral(2.5, exponent) - h(2.0, exponent),
            exponent,
        );

        ZipfSampler { n, exponent, h_integral_x1, h_integral_n, s }
    }

    /// Sample a key index in [0, n-1]. Index 0 is the hottest key.
    pub fn sample(&self, randsrc: &mut RandSrc) -> u64 {
        loop {
            let u = u64_to_f64_01(randsrc.get_uint64());
            let u_scaled =
                self.h_integral_x1 + u * (self.h_integral_n - self.h_integral_x1);
            let x = h_integral_inverse(u_scaled, self.exponent);

            let k = ((x + 0.5) as u64).max(1).min(self.n);
            let kf = k as f64;

            if kf - x <= self.s || u_scaled >= h_integral(kf + 0.5, self.exponent) {
                return k - 1; // [1, n] → [0, n-1]
            }
        }
    }
}

#[inline]
fn u64_to_f64_01(v: u64) -> f64 {
    (v >> 11) as f64 * (1.0f64 / (1u64 << 53) as f64)
}

/// h(x) = (x + 0.5)^(-exponent)
#[inline]
fn h(x: f64, exponent: f64) -> f64 {
    (-exponent * (x + 0.5).ln()).exp()
}

/// H(x) = (x + 0.5)^(1-exponent) / (1-exponent)
#[inline]
fn h_integral(x: f64, exponent: f64) -> f64 {
    let t = (1.0 - exponent) * (x + 0.5).ln();
    helper2(t) / (1.0 - exponent)
}

/// H^{-1}(y): solve y = (x+0.5)^(1-s)/(1-s) for x.
#[inline]
fn h_integral_inverse(y: f64, exponent: f64) -> f64 {
    let t = y * (1.0 - exponent);
    if t <= 0.0 {
        return -0.5;
    }
    (t.ln() / (1.0 - exponent)).exp() - 0.5
}

/// Numerically stable exp for small arguments.
#[inline]
fn helper2(x: f64) -> f64 {
    if x.abs() > 1e-8 {
        x.exp()
    } else {
        1.0 + x * (1.0 + x * (0.5 + x / 6.0))
    }
}
