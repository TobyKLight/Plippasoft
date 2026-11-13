//! MD5 (RFC 1321), written in-house.
//!
//! The only consumer is Subsonic token authentication: `t = md5(password + salt)`.
//! MD5 is broken as a collision-resistant hash, which is irrelevant here — the
//! protocol defines the function and both sides must agree. Do not use it for
//! anything else in Plippa.

/// Per-round left-rotation amounts.
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `floor(2^32 * abs(sin(i + 1)))` from RFC 1321.
const TABLE: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// Lowercase hexadecimal MD5 of `input`.
///
/// Crate-private on purpose (PAR-07): MD5 invites misuse, and the only legitimate use in
/// Plippa is [`subsonic_token`].
pub(crate) fn md5_hex(input: &[u8]) -> String {
    let digest = md5(input);
    let mut out = String::with_capacity(32);
    for byte in digest {
        use std::fmt::Write as _;
        // Writing into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The Subsonic token `t = md5(password + salt)`, the only MD5 use Plippa exposes.
///
/// This is the protocol's definition, not a security decision: the server stores no
/// salt history, so a captured token is equivalent to the password for the whole API.
/// Treat the result as a secret and never log the URL it travels in.
pub fn subsonic_token(password: &str, salt: &str) -> String {
    // One buffer, wiped when it is dropped: no `password + salt` copy survives.
    let joined = crate::credentials::SecretString::new(password).concat(salt);
    md5_hex(joined.expose().as_bytes())
}

/// Raw 16-byte MD5 digest of `input`.
pub(crate) fn md5(input: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(input.len() + 72);
    padded.extend_from_slice(input);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_le_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut words = [0u32; 16];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        }

        let [mut a, mut b, mut c, mut d] = state;
        for round in 0..64 {
            let (mix, word) = match round / 16 {
                0 => ((b & c) | (!b & d), round),
                1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
                2 => (b ^ c ^ d, (3 * round + 5) % 16),
                _ => (c ^ (b | !d), (7 * round) % 16),
            };
            let sum = a
                .wrapping_add(mix)
                .wrapping_add(TABLE[round])
                .wrapping_add(words[word]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(sum.rotate_left(SHIFTS[round]));
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
    }

    let mut digest = [0u8; 16];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_1321_test_suite() {
        let vectors = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            ("abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, expected) in vectors {
            assert_eq!(md5_hex(input.as_bytes()), expected, "md5({input:?})");
        }
    }

    #[test]
    fn subsonic_token_of_password_plus_salt() {
        // Same construction as the research doc's harness:
        // hashlib.md5((password + salt).encode("utf-8")).hexdigest()
        let password = "s3cr3tPassw0rd";
        let salt = "1a2b3c4d5e6f7788";
        assert_eq!(
            md5_hex(format!("{password}{salt}").as_bytes()),
            "8ad37da65da26c816c451fc4a45df585"
        );
        assert_eq!(md5_hex(b"password"), "5f4dcc3b5aa765d61d8327deb882cf99");
    }

    #[test]
    fn padding_boundaries_are_exact() {
        // 55, 56, 63 and 64 bytes exercise every padding and two-block case.
        let vectors = [
            (54, "eced9e0b81ef2bba605cbc5e2e76a1d0"),
            (55, "ef1772b6dff9a122358552954ad0df65"),
            (56, "3b0c8ac703f828b04c6c197006d17218"),
            (63, "b06521f39153d618550606be297466d5"),
            (64, "014842d480b571495a4a0363793f7367"),
        ];
        for (len, expected) in vectors {
            assert_eq!(md5_hex(&vec![b'a'; len]), expected, "{len} bytes");
        }
        // One byte set high exercises non-ASCII input.
        assert_eq!(md5_hex(&[0xff]), "00594fd4f42ba43fc1ca0427a0576295");
    }
}
