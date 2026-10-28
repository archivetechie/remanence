//! Runtime-selected split-nibble multiplication over GF(2^8)/0x11D.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
enum KernelKind {
    Portable,
    #[cfg(target_arch = "x86_64")]
    Ssse3,
    #[cfg(target_arch = "x86_64")]
    Avx2,
}

/// Only CPU detection constructs a callable SIMD kernel.
#[derive(Clone, Copy, Debug)]
pub(super) struct Kernel(KernelKind);

pub(super) fn selected() -> Kernel {
    static KERNEL: OnceLock<Kernel> = OnceLock::new();
    *KERNEL.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("avx2") {
                return Kernel(KernelKind::Avx2);
            }
            if is_x86_feature_detected!("ssse3") {
                return Kernel(KernelKind::Ssse3);
            }
        }
        Kernel(KernelKind::Portable)
    })
}

impl Kernel {
    pub(super) fn name(self) -> &'static str {
        match self.0 {
            KernelKind::Portable => "portable",
            #[cfg(target_arch = "x86_64")]
            KernelKind::Ssse3 => "ssse3",
            #[cfg(target_arch = "x86_64")]
            KernelKind::Avx2 => "avx2",
        }
    }

    pub(super) fn apply(self, coefficient: u8, input: &[u8], output: &mut [u8]) {
        assert_eq!(input.len(), output.len());
        if input.is_empty() || coefficient == 0 {
            return;
        }
        if coefficient == 1 {
            for (out, byte) in output.iter_mut().zip(input) {
                *out ^= *byte;
            }
            return;
        }
        match self.0 {
            KernelKind::Portable => portable(coefficient, input, output),
            #[cfg(target_arch = "x86_64")]
            KernelKind::Ssse3 => {
                // SAFETY: selection detects SSSE3; test selectors only include detected SSSE3.
                unsafe { ssse3(coefficient, input, output) }
            }
            #[cfg(target_arch = "x86_64")]
            KernelKind::Avx2 => {
                // SAFETY: selection detects AVX2; test selectors only include detected AVX2.
                unsafe { avx2(coefficient, input, output) }
            }
        }
    }
}

fn portable(coefficient: u8, input: &[u8], output: &mut [u8]) {
    reed_solomon_erasure::galois_8::mul_slice_xor(coefficient, input, output);
}

/// Separate low and high nibbles using the same field table as the codec.
#[cfg(target_arch = "x86_64")]
const fn nibble_tables() -> [[[u8; 16]; 2]; 256] {
    let mut tables = [[[0; 16]; 2]; 256];
    let mut c = 0;
    while c < 256 {
        let mut n = 0;
        while n < 16 {
            tables[c][0][n] = super::GF_MUL_TABLE[c][n];
            tables[c][1][n] = super::GF_MUL_TABLE[c][n << 4];
            n += 1;
        }
        c += 1;
    }
    tables
}
#[cfg(target_arch = "x86_64")]
const NIBBLES: [[[u8; 16]; 2]; 256] = nibble_tables();

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn ssse3(coefficient: u8, input: &[u8], output: &mut [u8]) {
    use std::arch::x86_64::*;
    let tables = &NIBBLES[coefficient as usize];
    // SAFETY: the caller detects SSSE3; both tables contain 16 bytes.
    let (low, high) = unsafe {
        (
            _mm_loadu_si128(tables[0].as_ptr().cast()),
            _mm_loadu_si128(tables[1].as_ptr().cast()),
        )
    };
    let mask = _mm_set1_epi8(15);
    let end = input.len() / 16 * 16;
    for offset in (0..end).step_by(16) {
        // SAFETY: the caller detects SSSE3; equal-length slices contain
        // this complete 16-byte chunk, and unaligned loads/stores are permitted.
        unsafe {
            let bytes = _mm_loadu_si128(input.as_ptr().add(offset).cast::<__m128i>());
            let lo = _mm_and_si128(bytes, mask);
            let hi = _mm_and_si128(_mm_srli_epi16::<4>(bytes), mask);
            let product = _mm_xor_si128(_mm_shuffle_epi8(low, lo), _mm_shuffle_epi8(high, hi));
            let ptr = output.as_mut_ptr().add(offset).cast::<__m128i>();
            _mm_storeu_si128(ptr, _mm_xor_si128(_mm_loadu_si128(ptr), product));
        }
    }
    portable(coefficient, &input[end..], &mut output[end..]);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2(coefficient: u8, input: &[u8], output: &mut [u8]) {
    use std::arch::x86_64::*;
    let tables = &NIBBLES[coefficient as usize];
    // SAFETY: the caller detects AVX2; both tables contain 16 bytes.
    let (low, high) = unsafe {
        (
            _mm256_broadcastsi128_si256(_mm_loadu_si128(tables[0].as_ptr().cast())),
            _mm256_broadcastsi128_si256(_mm_loadu_si128(tables[1].as_ptr().cast())),
        )
    };
    let mask = _mm256_set1_epi8(15);
    let end = input.len() / 32 * 32;
    for offset in (0..end).step_by(32) {
        // SAFETY: the caller detects AVX2; equal-length slices contain
        // this complete 32-byte chunk, and unaligned loads/stores are permitted.
        unsafe {
            let bytes = _mm256_loadu_si256(input.as_ptr().add(offset).cast::<__m256i>());
            let lo = _mm256_and_si256(bytes, mask);
            let hi = _mm256_and_si256(_mm256_srli_epi16::<4>(bytes), mask);
            let product =
                _mm256_xor_si256(_mm256_shuffle_epi8(low, lo), _mm256_shuffle_epi8(high, hi));
            let ptr = output.as_mut_ptr().add(offset).cast::<__m256i>();
            _mm256_storeu_si256(ptr, _mm256_xor_si256(_mm256_loadu_si256(ptr), product));
        }
    }
    portable(coefficient, &input[end..], &mut output[end..]);
}

/// Test-only selector exercises every implementation available on this host.
#[cfg(test)]
pub(super) fn supported() -> Vec<Kernel> {
    #[allow(unused_mut)]
    let mut kernels = vec![Kernel(KernelKind::Portable)];
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("ssse3") {
            kernels.push(Kernel(KernelKind::Ssse3));
        }
        if is_x86_feature_detected!("avx2") {
            kernels.push(Kernel(KernelKind::Avx2));
        }
    }
    kernels
}
