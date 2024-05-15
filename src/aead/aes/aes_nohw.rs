// Copyright (c) 2019, Google Inc.
// Portions Copyright 2024 Brian Smith.
//
// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
// SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

use super::{Counter, KeyBytes, AES_KEY, BLOCK_LEN, MAX_ROUNDS};
use crate::{
    c, constant_time,
    polyfill::{self, usize_from_u32, ArraySplitMap as _},
};
use core::{array, mem::MaybeUninit, ops::RangeFrom};

type Word = constant_time::Word;
const WORD_SIZE: usize = core::mem::size_of::<Word>();
const BATCH_SIZE: usize = WORD_SIZE / 2;
#[allow(clippy::cast_possible_truncation)]
const BATCH_SIZE_U32: u32 = BATCH_SIZE as u32;

const BLOCK_WORDS: usize = 16 / WORD_SIZE;

fn compact_block(input: &[u8; 16]) -> [Word; BLOCK_WORDS] {
    prefixed_extern! {
        fn aes_nohw_compact_block(out: *mut [Word; BLOCK_WORDS], input: &[u8; 16]);
    }
    let mut block = MaybeUninit::uninit();
    unsafe {
        aes_nohw_compact_block(block.as_mut_ptr(), input);
        block.assume_init()
    }
}

// An AES_NOHW_BATCH stores |AES_NOHW_BATCH_SIZE| blocks. Unless otherwise
// specified, it is in bitsliced form.
#[repr(C)]
struct Batch {
    w: [Word; 8],
}

impl Batch {
    // aes_nohw_to_batch initializes |out| with the |num_blocks| blocks from |in|.
    // |num_blocks| must be at most |AES_NOHW_BATCH|.
    fn from_bytes(input: &[[u8; BLOCK_LEN]]) -> Self {
        let mut r = Self {
            w: Default::default(),
        };
        input.iter().enumerate().for_each(|(i, input)| {
            let block = compact_block(input);
            r.set(&block, i);
        });
        r.transpose();
        r
    }

    // aes_nohw_batch_set sets the |i|th block of |batch| to |in|. |batch| is in
    // compact form.
    fn set(&mut self, input: &[Word; BLOCK_WORDS], i: usize) {
        prefixed_extern! {
            fn aes_nohw_batch_set(batch: *mut Batch, input: &[Word; BLOCK_WORDS], i: usize);
        }
        unsafe { aes_nohw_batch_set(self, input, i) }
    }

    fn encrypt(mut self, key: &Schedule, rounds: usize, out: &mut [[u8; BLOCK_LEN]]) {
        assert!(out.len() <= BATCH_SIZE);
        prefixed_extern! {
            fn aes_nohw_encrypt_batch(key: &Schedule, num_rounds: usize, batch: &mut Batch);
            fn aes_nohw_from_batch(out: *mut [u8; BLOCK_LEN], num_blocks: c::size_t, batch: &Batch);
        }
        unsafe {
            aes_nohw_encrypt_batch(key, rounds, &mut self);
            aes_nohw_from_batch(out.as_mut_ptr(), out.len(), &self);
        }
    }

    fn transpose(&mut self) {
        prefixed_extern! {
            fn aes_nohw_transpose(batch: &mut Batch);
        }
        unsafe { aes_nohw_transpose(self) }
    }
}

// Key schedule.

// An AES_NOHW_SCHEDULE is an expanded bitsliced AES key schedule. It is
// suitable for encryption or decryption. It is as large as |AES_NOHW_BATCH|
// |AES_KEY|s so it should not be used as a long-term key representation.
#[repr(C)]
struct Schedule {
    // keys is an array of batches, one for each round key. Each batch stores
    // |AES_NOHW_BATCH_SIZE| copies of the round key in bitsliced form.
    keys: [Batch; MAX_ROUNDS + 1],
}

impl Schedule {
    fn expand_round_keys(key: &AES_KEY) -> Self {
        Self {
            keys: array::from_fn(|i| {
                let tmp: [Word; BLOCK_WORDS] = unsafe { core::mem::transmute(key.rd_key[i]) };

                let mut r = Batch { w: [0; 8] };
                // Copy the round key into each block in the batch.
                for j in 0..BATCH_SIZE {
                    r.set(&tmp, j);
                }
                r.transpose();
                r
            }),
        }
    }
}

pub(super) fn set_encrypt_key(key: &mut AES_KEY, bytes: KeyBytes) {
    prefixed_extern! {
        fn aes_nohw_setup_key_128(key: *mut AES_KEY, input: &[u8; 128 / 8]);
        fn aes_nohw_setup_key_256(key: *mut AES_KEY, input: &[u8; 256 / 8]);
    }
    match bytes {
        KeyBytes::AES_128(bytes) => unsafe { aes_nohw_setup_key_128(key, bytes) },
        KeyBytes::AES_256(bytes) => unsafe { aes_nohw_setup_key_256(key, bytes) },
    }
}

pub(super) fn encrypt_block(key: &AES_KEY, in_out: &mut [u8; BLOCK_LEN]) {
    let sched = Schedule::expand_round_keys(key);
    let batch = Batch::from_bytes(core::slice::from_ref(in_out));
    batch.encrypt(&sched, usize_from_u32(key.rounds), array::from_mut(in_out));
}

pub(super) fn ctr32_encrypt_within(
    key: &AES_KEY,
    mut in_out: &mut [u8],
    src: RangeFrom<usize>,
    ctr: &mut Counter,
) {
    let (input, leftover): (&[[u8; BLOCK_LEN]], _) =
        polyfill::slice::as_chunks(&in_out[src.clone()]);
    debug_assert_eq!(leftover.len(), 0);
    if input.is_empty() {
        return;
    }
    let blocks_u32 = u32::try_from(input.len()).unwrap();

    let sched = Schedule::expand_round_keys(key);

    let initial_ctr = ctr.as_bytes_less_safe();
    ctr.increment_by_less_safe(blocks_u32);

    let mut ivs = [initial_ctr; BATCH_SIZE];
    let mut enc_ctrs = [[0u8; 16]; BATCH_SIZE];
    let initial_ctr: [[u8; 4]; 4] = initial_ctr.array_split_map(|x| x);
    let mut ctr = u32::from_be_bytes(initial_ctr[3]);

    for _ in (0..).step_by(BATCH_SIZE) {
        (0u32..).zip(ivs.iter_mut()).for_each(|(i, iv)| {
            iv[12..].copy_from_slice(&u32::to_be_bytes(ctr + i));
        });

        let (input, leftover): (&[[u8; BLOCK_LEN]], _) =
            polyfill::slice::as_chunks(&in_out[src.clone()]);
        debug_assert_eq!(leftover.len(), 0);
        let todo = core::cmp::min(ivs.len(), input.len());
        let batch = Batch::from_bytes(&ivs[..todo]);
        batch.encrypt(&sched, usize_from_u32(key.rounds), &mut enc_ctrs[..todo]);
        constant_time::xor_within_chunked_at_start(in_out, src.clone(), &enc_ctrs[..todo]);

        if todo < BATCH_SIZE {
            break;
        }
        in_out = &mut in_out[(BLOCK_LEN * BATCH_SIZE)..];
        ctr += BATCH_SIZE_U32;
    }
}
