//! [AES-GCM-SIV][1] ([RFC 8452][2]): high-performance
//! [Authenticated Encryption with Associated Data (AEAD)][3] cipher which also
//! provides [nonce reuse misuse resistance][4].
//!
//! [1]: https://en.wikipedia.org/wiki/AES-GCM-SIV
//! [2]: https://tools.ietf.org/html/rfc8452
//! [3]: https://en.wikipedia.org/wiki/Authenticated_encryption
//! [4]: https://github.com/miscreant/meta/wiki/Nonce-Reuse-Misuse-Resistance

#![no_std]

extern crate alloc;

pub use aead;

use aead::generic_array::{
    typenum::{Unsigned, U0, U12, U16, U8},
    GenericArray,
};
use aead::{Aead, Error, NewAead, Payload};
use aes::{block_cipher_trait::BlockCipher, Aes128, Aes256};
use alloc::vec::Vec;
use core::{convert::TryInto, marker::PhantomData};
use polyval::{universal_hash::UniversalHash, Polyval};

/// Maximum length of associated data (from RFC 8452 Section 6)
pub const A_MAX: u64 = 1 << 36;

/// Maximum length of plaintext (from RFC 8452 Section 6)
pub const P_MAX: u64 = 1 << 36;

/// Maximum length of ciphertext (from RFC 8452 Section 6)
pub const C_MAX: u64 = (1 << 36) + 16;

/// AES-GCM-SIV tags
type Tag = GenericArray<u8, U16>;

/// AES-GCM-SIV with a 128-bit key
pub type Aes128GcmSiv = AesGcmSiv<Aes128>;

/// AES-GCM-SIV with a 256-bit key
pub type Aes256GcmSiv = AesGcmSiv<Aes256>;

/// AES-GCM-SIV: Misuse-Resistant Authenticated Encryption Cipher (RFC 8452)
#[derive(Clone)]
pub struct AesGcmSiv<C: BlockCipher<BlockSize = U16, ParBlocks = U8>> {
    /// Secret key
    key: GenericArray<u8, C::KeySize>,

    /// AES block cipher
    block_cipher: PhantomData<C>,
}

impl<C> NewAead for AesGcmSiv<C>
where
    C: BlockCipher<BlockSize = U16, ParBlocks = U8>,
{
    type KeySize = C::KeySize;

    fn new(key: GenericArray<u8, C::KeySize>) -> Self {
        Self {
            key,
            block_cipher: PhantomData,
        }
    }
}

impl<C> Aead for AesGcmSiv<C>
where
    C: BlockCipher<BlockSize = U16, ParBlocks = U8>,
{
    type NonceSize = U12;
    type TagSize = U16;
    type CiphertextOverhead = U0;

    fn encrypt<'msg, 'aad>(
        &self,
        nonce: &GenericArray<u8, Self::NonceSize>,
        plaintext: impl Into<Payload<'msg, 'aad>>,
    ) -> Result<Vec<u8>, Error> {
        Cipher::<C>::new(&self.key, nonce).encrypt(plaintext.into())
    }

    fn decrypt<'msg, 'aad>(
        &self,
        nonce: &GenericArray<u8, Self::NonceSize>,
        ciphertext: impl Into<Payload<'msg, 'aad>>,
    ) -> Result<Vec<u8>, Error> {
        Cipher::<C>::new(&self.key, nonce).decrypt(ciphertext.into())
    }
}

/// AES-GCM-SIV: Misuse-Resistant Authenticated Encryption Cipher (RFC 8452)
struct Cipher<C: BlockCipher<BlockSize = U16, ParBlocks = U8>> {
    /// Encryption cipher
    enc_cipher: C,

    /// POLYVAL universal hash
    polyval: Polyval,

    /// Nonce
    nonce: GenericArray<u8, U12>,
}

impl<C> Cipher<C>
where
    C: BlockCipher<BlockSize = U16, ParBlocks = U8>,
{
    /// Initialize AES-GCM-SIV, deriving per-nonce message-authentication and
    /// message-encryption keys.
    pub(crate) fn new(key: &GenericArray<u8, C::KeySize>, nonce: &GenericArray<u8, U12>) -> Self {
        let key_generating_key = C::new(key);

        // TODO(tarcieri): zeroize all of these buffers!
        let mut mac_key = GenericArray::default();
        let mut enc_key = GenericArray::default();
        let mut block = GenericArray::default();
        let mut counter = 0u32;

        // Derive subkeys from the master key-generating-key in counter mode.
        //
        // From RFC 8452 Section 4:
        // <https://tools.ietf.org/html/rfc8452#section-4>
        //
        // > The message-authentication key is 128 bit, and the message-encryption
        // > key is either 128 (for AES-128) or 256 bit (for AES-256).
        // >
        // > These keys are generated by encrypting a series of plaintext blocks
        // > that contain a 32-bit, little-endian counter followed by the nonce,
        // > and then discarding the second half of the resulting ciphertext.  In
        // > the AES-128 case, 128 + 128 = 256 bits of key material need to be
        // > generated, and, since encrypting each block yields 64 bits after
        // > discarding half, four blocks need to be encrypted.  The counter
        // > values for these blocks are 0, 1, 2, and 3.  For AES-256, six blocks
        // > are needed in total, with counter values 0 through 5 (inclusive).
        for derived_key in &mut [mac_key.as_mut(), enc_key.as_mut()] {
            for chunk in derived_key.chunks_mut(8) {
                block[..4].copy_from_slice(&counter.to_le_bytes());
                block[4..].copy_from_slice(nonce.as_slice());

                key_generating_key.encrypt_block(&mut block);
                chunk.copy_from_slice(&block.as_slice()[..8]);

                counter += 1;
            }
        }

        Self {
            enc_cipher: C::new(&enc_key),
            polyval: Polyval::new(&mac_key),
            nonce: *nonce,
        }
    }

    /// Encrypt the given message, allocating a vector for the resulting ciphertext
    pub(crate) fn encrypt(self, payload: Payload) -> Result<Vec<u8>, Error> {
        let tag_size = <Polyval as UniversalHash>::OutputSize::to_usize();

        let mut buffer = Vec::with_capacity(payload.msg.len() + tag_size);
        buffer.extend_from_slice(payload.msg);

        let tag = self.encrypt_in_place(&mut buffer, payload.aad)?;
        buffer.extend_from_slice(tag.as_slice());
        Ok(buffer)
    }

    /// Encrypt the given message in-place, returning the authentication tag
    pub(crate) fn encrypt_in_place(
        mut self,
        buffer: &mut [u8],
        associated_data: &[u8],
    ) -> Result<Tag, Error> {
        if buffer.len() as u64 > P_MAX || associated_data.len() as u64 > A_MAX {
            return Err(Error);
        }

        let tag = self.compute_tag(buffer, associated_data);
        self.ctr32le(tag, buffer);
        Ok(tag)
    }

    /// Decrypt the given message, allocating a vector for the resulting plaintext
    pub(crate) fn decrypt(self, payload: Payload) -> Result<Vec<u8>, Error> {
        let tag_size = <Polyval as UniversalHash>::OutputSize::to_usize();

        if payload.msg.len() < tag_size {
            return Err(Error);
        }

        let tag_start = payload.msg.len() - tag_size;
        let mut buffer = Vec::from(&payload.msg[..tag_start]);
        let tag = GenericArray::from_slice(&payload.msg[tag_start..]);
        self.decrypt_in_place(&mut buffer, payload.aad, *tag)?;

        Ok(buffer)
    }

    /// Decrypt the given message, first authenticating ciphertext integrity
    /// and returning an error if it's been tampered with.
    pub(crate) fn decrypt_in_place(
        mut self,
        buffer: &mut [u8],
        associated_data: &[u8],
        tag: Tag,
    ) -> Result<(), Error> {
        if buffer.len() as u64 > C_MAX || associated_data.len() as u64 > A_MAX {
            return Err(Error);
        }

        self.ctr32le(tag, buffer);
        let expected_tag = self.compute_tag(buffer, associated_data);

        use subtle::ConstantTimeEq;
        if expected_tag.ct_eq(&tag).unwrap_u8() == 1 {
            Ok(())
        } else {
            // On MAC verify failure, re-encrypt the plaintext buffer to
            // prevent accidental exposure.
            self.ctr32le(tag, buffer);
            Err(Error)
        }
    }

    /// Authenticate the given plaintext and associated data
    fn compute_tag(&mut self, buffer: &mut [u8], associated_data: &[u8]) -> Tag {
        self.polyval.update_padded(associated_data);
        self.polyval.update_padded(buffer);

        let associated_data_len = (associated_data.len() as u64) * 8;
        let buffer_len = (buffer.len() as u64) * 8;

        let mut block = GenericArray::default();
        block[..8].copy_from_slice(&associated_data_len.to_le_bytes());
        block[8..].copy_from_slice(&buffer_len.to_le_bytes());
        self.polyval.update_block(&block);

        let mut tag = self.polyval.result_reset().into_bytes();

        // XOR the nonce into the resulting tag
        for (i, byte) in tag[..12].iter_mut().enumerate() {
            *byte ^= self.nonce[i];
        }

        tag[15] &= 0x7f;

        self.enc_cipher.encrypt_block(&mut tag);
        tag
    }

    /// CTR mode with a 32-bit little endian counter
    fn ctr32le(&self, mut counter_block: GenericArray<u8, U16>, buffer: &mut [u8]) {
        counter_block[15] |= 0x80;

        for chunk in buffer.chunks_mut(C::BlockSize::to_usize()) {
            let mut keystream_block = counter_block;
            self.enc_cipher.encrypt_block(&mut keystream_block);

            // Increment counter
            let counter =
                u32::from_le_bytes(counter_block[..4].try_into().unwrap()).wrapping_add(1);

            counter_block[..4].copy_from_slice(&counter.to_le_bytes());

            for (i, byte) in chunk.iter_mut().enumerate() {
                *byte ^= keystream_block[i];
            }
        }
    }
}
