// Copyright 2018 Brian Smith.
//
// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHORS DISCLAIM ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY
// SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

use super::{aes_gcm, Aad};

use crate::{
    bits::{BitLength, FromByteLen as _},
    constant_time, cpu, error,
    polyfill::{nonempty, sliceutil::overwrite_at_start, ArrayFlatten as _, ArraySplitMap as _},
};
use core::{num::NonZeroUsize, ops::BitXorAssign};

// GCM uses the same block type as AES.
use super::aes::{Block, BLOCK_LEN, ZERO_BLOCK};

mod gcm_nohw;

#[derive(Clone)]
pub struct Key {
    h_table: HTable,
}

impl Key {
    pub(super) fn new(h_be: Block, cpu_features: cpu::Features) -> Self {
        let h: [u64; 2] = h_be.array_split_map(u64::from_be_bytes);

        let mut key = Self {
            h_table: HTable {
                Htable: [u128 { hi: 0, lo: 0 }; HTABLE_LEN],
            },
        };
        let h_table = &mut key.h_table;

        match detect_implementation(cpu_features) {
            #[cfg(target_arch = "x86_64")]
            Implementation::CLMUL if has_avx_movbe(cpu_features) => {
                prefixed_extern! {
                    fn gcm_init_avx(HTable: &mut HTable, h: &[u64; 2]);
                }
                unsafe {
                    gcm_init_avx(h_table, &h);
                }
            }

            #[cfg(any(
                target_arch = "aarch64",
                target_arch = "arm",
                target_arch = "x86_64",
                target_arch = "x86"
            ))]
            Implementation::CLMUL => {
                prefixed_extern! {
                    fn gcm_init_clmul(Htable: &mut HTable, h: &[u64; 2]);
                }
                unsafe {
                    gcm_init_clmul(h_table, &h);
                }
            }

            #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
            Implementation::NEON => {
                prefixed_extern! {
                    fn gcm_init_neon(Htable: &mut HTable, h: &[u64; 2]);
                }
                unsafe {
                    gcm_init_neon(h_table, &h);
                }
            }

            Implementation::Fallback => {
                h_table.Htable[0] = gcm_nohw::init(h);
            }
        }

        key
    }
}

pub struct Context {
    inner: ContextInner,
    aad_len: BitLength<u64>,
    in_out_len: BitLength<u64>,
    cpu_features: cpu::Features,
}

impl Context {
    pub(crate) fn new(
        key: &Key,
        aad: Aad<&[u8]>,
        in_out_len: usize,
        cpu_features: cpu::Features,
    ) -> Result<Self, error::Unspecified> {
        if in_out_len > aes_gcm::MAX_IN_OUT_LEN {
            return Err(error::Unspecified);
        }

        // NIST SP800-38D Section 5.2.1.1 says that the maximum AAD length is
        // 2**64 - 1 bits, i.e. BitLength<u64>::MAX, so we don't need to do an
        // explicit check here.

        let mut ctx = Self {
            inner: ContextInner {
                Xi: Xi(ZERO_BLOCK),
                Htable: key.h_table.clone(),
            },
            aad_len: BitLength::from_byte_len(aad.as_ref().len())?,
            in_out_len: BitLength::from_byte_len(in_out_len)?,
            cpu_features,
        };

        for ad in aad.0.chunks(BLOCK_LEN) {
            let mut block = ZERO_BLOCK;
            overwrite_at_start(&mut block, ad);
            ctx.update_block(block);
        }

        Ok(ctx)
    }

    /// Access to `inner` for the integrated AES-GCM implementations only.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    #[inline]
    pub(super) fn inner(&mut self) -> (&HTable, &mut Xi) {
        (&self.inner.Htable, &mut self.inner.Xi)
    }

    pub fn update_blocks(&mut self, input: nonempty::Slice<[u8; BLOCK_LEN]>) {
        // The assembly functions take the input length in bytes, not blocks.
        // TODO: document overflow assumptions.
        let input_bytes = NonZeroUsize::new(input.len().get() * BLOCK_LEN).unwrap();

        let xi = &mut self.inner.Xi;
        let h_table = &self.inner.Htable;

        match detect_implementation(self.cpu_features) {
            #[cfg(target_arch = "x86_64")]
            Implementation::CLMUL if has_avx_movbe(self.cpu_features) => {
                prefixed_extern! {
                    fn gcm_ghash_avx(
                        xi: &mut Xi,
                        Htable: &HTable,
                        inp: *const [u8; BLOCK_LEN],
                        len: crate::c::NonZero_size_t,
                    );
                }
                unsafe {
                    gcm_ghash_avx(xi, h_table, input.as_ptr(), input_bytes);
                }
            }

            #[cfg(any(
                target_arch = "aarch64",
                target_arch = "arm",
                target_arch = "x86_64",
                target_arch = "x86"
            ))]
            Implementation::CLMUL => {
                prefixed_extern! {
                    fn gcm_ghash_clmul(
                        xi: &mut Xi,
                        Htable: &HTable,
                        inp: *const [u8; BLOCK_LEN],
                        len: crate::c::NonZero_size_t,
                    );
                }
                unsafe {
                    gcm_ghash_clmul(xi, h_table, input.as_ptr(), input_bytes);
                }
            }

            #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
            Implementation::NEON => {
                prefixed_extern! {
                    fn gcm_ghash_neon(
                        xi: &mut Xi,
                        Htable: &HTable,
                        inp: *const [u8; BLOCK_LEN],
                        len: crate::c::NonZero_size_t,
                    );
                }
                unsafe {
                    gcm_ghash_neon(xi, h_table, input.as_ptr(), input_bytes);
                }
            }

            Implementation::Fallback => {
                gcm_nohw::ghash(xi, h_table.Htable[0], input.into());
            }
        }
    }

    pub fn update_block(&mut self, a: Block) {
        self.inner.Xi.bitxor_assign(a);

        // Although these functions take `Xi` and `h_table` as separate
        // parameters, one or more of them might assume that they are part of
        // the same `ContextInner` structure.
        let xi = &mut self.inner.Xi;
        let h_table = &self.inner.Htable;

        match detect_implementation(self.cpu_features) {
            #[cfg(any(
                target_arch = "aarch64",
                target_arch = "arm",
                target_arch = "x86_64",
                target_arch = "x86"
            ))]
            Implementation::CLMUL => {
                prefixed_extern! {
                    fn gcm_gmult_clmul(xi: &mut Xi, Htable: &HTable);
                }
                unsafe {
                    gcm_gmult_clmul(xi, h_table);
                }
            }

            #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
            Implementation::NEON => {
                prefixed_extern! {
                    fn gcm_gmult_neon(xi: &mut Xi, Htable: &HTable);
                }
                unsafe {
                    gcm_gmult_neon(xi, h_table);
                }
            }

            Implementation::Fallback => {
                gcm_nohw::gmult(xi, h_table.Htable[0]);
            }
        }
    }

    pub(super) fn pre_finish<F>(mut self, f: F) -> super::Tag
    where
        F: FnOnce(Block, cpu::Features) -> super::Tag,
    {
        self.update_block(
            [self.aad_len, self.in_out_len]
                .map(BitLength::to_be_bytes)
                .array_flatten(),
        );

        f(self.inner.Xi.0, self.cpu_features)
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn is_avx(&self) -> bool {
        match detect_implementation(self.cpu_features) {
            Implementation::CLMUL => has_avx_movbe(self.cpu_features),
            _ => false,
        }
    }

    #[cfg(target_arch = "aarch64")]
    pub(super) fn is_clmul(&self) -> bool {
        matches!(
            detect_implementation(self.cpu_features),
            Implementation::CLMUL
        )
    }
}

// The alignment is required by non-Rust code that uses `GCM128_CONTEXT`.
#[derive(Clone)]
#[repr(C, align(16))]
pub(super) struct HTable {
    Htable: [u128; HTABLE_LEN],
}

#[derive(Clone, Copy)]
#[repr(C)]
struct u128 {
    hi: u64,
    lo: u64,
}

const HTABLE_LEN: usize = 16;

#[repr(transparent)]
pub struct Xi(Block);

impl BitXorAssign<Block> for Xi {
    #[inline]
    fn bitxor_assign(&mut self, a: Block) {
        self.0 = constant_time::xor(self.0, a)
    }
}

// This corresponds roughly to the `GCM128_CONTEXT` structure in BoringSSL.
// Some assembly language code, in particular the MOVEBE+AVX2 X86-64
// implementation, requires this exact layout.
#[repr(C, align(16))]
struct ContextInner {
    Xi: Xi,
    Htable: HTable,
}

#[allow(clippy::upper_case_acronyms)]
enum Implementation {
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "x86_64",
        target_arch = "x86"
    ))]
    CLMUL,

    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    NEON,

    Fallback,
}

#[inline]
fn detect_implementation(cpu_features: cpu::Features) -> Implementation {
    // `cpu_features` is only used for specific platforms.
    #[cfg(not(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "x86_64",
        target_arch = "x86"
    )))]
    let _cpu_features = cpu_features;

    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    {
        if cpu::arm::PMULL.available(cpu_features) {
            return Implementation::CLMUL;
        }
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        if cpu::intel::FXSR.available(cpu_features) && cpu::intel::PCLMULQDQ.available(cpu_features)
        {
            return Implementation::CLMUL;
        }
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    {
        if cpu::arm::NEON.available(cpu_features) {
            return Implementation::NEON;
        }
    }

    Implementation::Fallback
}

#[cfg(target_arch = "x86_64")]
fn has_avx_movbe(cpu_features: cpu::Features) -> bool {
    cpu::intel::AVX.available(cpu_features) && cpu::intel::MOVBE.available(cpu_features)
}
