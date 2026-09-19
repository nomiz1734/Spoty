# Vendored `opus-decoder` 0.1.1

Upstream: https://crates.io/crates/opus-decoder (license: MIT OR Apache-2.0).

Spoty change: `src/celt/kiss_fft.rs::flat_fft_forward` was an O(N²) DFT that
evaluated sin/cos for every term, which is far slower than real time on the
TrimUI's Cortex-A53. It now uses a cached `rustfft` plan (same unscaled forward
transform). The test targets and dev-dependencies were dropped from Cargo.toml.
