// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cmp::Ordering;

use tee_crypto::bignum::*;

#[test]
fn test_bignum_zero() {
    let bn = TeeBigNum::from_bytes(&[0]).unwrap();
    assert_eq!(bn.bit_length(), 0);
}

#[test]
fn test_bignum_from_bytes_leading_zeros() {
    let bn = TeeBigNum::from_bytes(&[0x00, 0x00, 0x01]).unwrap();
    assert_eq!(bn.to_bytes().unwrap(), &[0x01]);
}

#[test]
fn test_bignum_bit_length_0x01_0x00() {
    let bn = TeeBigNum::from_bytes(&[0x01, 0x00]).unwrap();
    assert_eq!(bn.bit_length(), 9);
}

#[test]
fn test_bignum_bit_length_various() {
    let bn = TeeBigNum::from_bytes(&[0x01]).unwrap();
    assert_eq!(bn.bit_length(), 1);
    let bn = TeeBigNum::from_bytes(&[0xFF]).unwrap();
    assert_eq!(bn.bit_length(), 8);
    let bn = TeeBigNum::from_bytes(&[0x01, 0x00]).unwrap();
    assert_eq!(bn.bit_length(), 9);
    let bn = TeeBigNum::from_bytes(&[0xFF, 0x00]).unwrap();
    assert_eq!(bn.bit_length(), 16);
    let bn = TeeBigNum::from_bytes(&[0x80]).unwrap();
    assert_eq!(bn.bit_length(), 8);
}

#[test]
fn test_bignum_compare_less() {
    let a = TeeBigNum::from_bytes(&[0x01]).unwrap();
    let b = TeeBigNum::from_bytes(&[0x02]).unwrap();
    assert_eq!(a.compare(&b), Ordering::Less);
}

#[test]
fn test_bignum_compare_greater() {
    let a = TeeBigNum::from_bytes(&[0x03]).unwrap();
    let b = TeeBigNum::from_bytes(&[0x02]).unwrap();
    assert_eq!(a.compare(&b), Ordering::Greater);
}

#[test]
fn test_bignum_compare_equal() {
    let a = TeeBigNum::from_bytes(&[0x01, 0x00]).unwrap();
    let b = TeeBigNum::from_bytes(&[0x01, 0x00]).unwrap();
    assert_eq!(a.compare(&b), Ordering::Equal);
}

#[test]
fn test_bignum_compare_different_lengths() {
    let a = TeeBigNum::from_bytes(&[0x01]).unwrap();
    let b = TeeBigNum::from_bytes(&[0x01, 0x00]).unwrap();
    assert_eq!(a.compare(&b), Ordering::Less);
    assert_eq!(b.compare(&a), Ordering::Greater);
}

#[test]
fn test_bignum_compare_zero() {
    let a = TeeBigNum::from_bytes(&[0x00]).unwrap();
    let b = TeeBigNum::from_bytes(&[0x00]).unwrap();
    assert_eq!(a.compare(&b), Ordering::Equal);
    let c = TeeBigNum::from_bytes(&[0x01]).unwrap();
    assert_eq!(a.compare(&c), Ordering::Less);
}

#[test]
fn test_bignum_to_bytes_roundtrip() {
    let original = &[0xDE, 0xAD, 0xBE, 0xEF];
    let bn = TeeBigNum::from_bytes(original).unwrap();
    let bytes = bn.to_bytes().unwrap();
    assert_eq!(&bytes[..], original);
}

#[test]
fn test_bignum_to_bytes_roundtrip_leading_zeros() {
    let original = &[0x00, 0x00, 0xDE, 0xAD];
    let bn = TeeBigNum::from_bytes(original).unwrap();
    let bytes = bn.to_bytes().unwrap();
    assert_eq!(&bytes[..], &[0xDE, 0xAD]);
}

#[test]
fn test_bignum_new() {
    let bn = TeeBigNum::new();
    assert_eq!(bn.bit_length(), 0);
    assert_eq!(bn.to_bytes().unwrap(), &[0]);
}

#[test]
fn test_bignum_allocate() {
    let bn = TeeBigNum::allocate(128);
    assert_eq!(bn.byte_length(), 1);
    assert_eq!(bn.bit_length(), 0);
}

#[test]
fn test_bignum_allocate_odd_bits() {
    let bn = TeeBigNum::allocate(17);
    assert_eq!(bn.byte_length(), 1);
}

#[test]
fn test_bignum_from_bytes_empty_fails() {
    let result = TeeBigNum::from_bytes(&[]);
    assert!(result.is_err());
}

#[test]
fn test_bignum_default() {
    let bn = TeeBigNum::default();
    assert_eq!(bn.bit_length(), 0);
}

#[test]
fn test_byte_length() {
    let bn = TeeBigNum::from_bytes(&[0x01, 0x00]).unwrap();
    assert_eq!(bn.byte_length(), 2);
    let zero = TeeBigNum::new();
    assert_eq!(zero.byte_length(), 1);
}

#[test]
fn test_clear() {
    let mut bn = TeeBigNum::from_bytes(&[0xFF]).unwrap();
    bn.clear();
    assert_eq!(bn.to_bytes().unwrap(), &[0]);
}

#[test]
fn test_as_u32() {
    let bn = TeeBigNum::from_bytes(&[0x00, 0x00, 0x56, 0x78]).unwrap();
    assert_eq!(bn.as_u32().unwrap(), 0x5678);
}

#[test]
fn test_bigint_from_i32_roundtrip() {
    for value in [0, 1, -1, i32::MAX, i32::MIN] {
        let bigint = TeeBigInt::from_i32(value);
        assert_eq!(bigint.to_i32().unwrap(), value);
    }
}

#[test]
fn test_bigint_sign_bytes() {
    let bigint = TeeBigInt::from_sign_bytes(-1, &[0x01, 0x00]).unwrap();
    assert!(bigint.is_negative());
    assert_eq!(bigint.magnitude_bytes(), &[0x01, 0x00]);
    assert_eq!(bigint.sign_i32(), -1);
}

#[test]
fn test_bigint_u32_limbs_roundtrip() {
    let bigint = TeeBigInt::from_u32_le_limbs(-1, &[0x89ab_cdef, 0x0123_4567]);
    assert_eq!(bigint.sign_i32(), -1);
    assert_eq!(bigint.magnitude_u32_le(), &[0x89ab_cdef, 0x0123_4567]);
    assert_eq!(
        bigint.magnitude_bytes(),
        &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]
    );
}

#[test]
fn test_bigint_add_sub_signed() {
    let a = TeeBigInt::from_i32(100);
    let b = TeeBigInt::from_i32(-42);
    assert_eq!(a.add(&b).to_i32().unwrap(), 58);
    assert_eq!(b.sub(&a).to_i32().unwrap(), -142);
}

#[test]
fn test_bigint_mul_div_rem_signed() {
    let a = TeeBigInt::from_i32(-25);
    let b = TeeBigInt::from_i32(4);
    assert_eq!(a.mul(&b).to_i32().unwrap(), -100);
    let (q, r) = a.div_rem(&b).unwrap();
    assert_eq!(q.to_i32().unwrap(), -6);
    assert_eq!(r.to_i32().unwrap(), -1);
}

#[test]
fn test_bigint_modular_ops() {
    let a = TeeBigInt::from_i32(5);
    let b = TeeBigInt::from_i32(3);
    let n = TeeBigInt::from_i32(7);
    assert_eq!(a.add_mod(&b, &n).unwrap().to_i32().unwrap(), 1);
    assert_eq!(a.sub_mod(&b, &n).unwrap().to_i32().unwrap(), 2);
    assert_eq!(a.mul_mod(&b, &n).unwrap().to_i32().unwrap(), 1);
    assert_eq!(a.square_mod(&n).unwrap().to_i32().unwrap(), 4);
}

#[test]
fn test_bigint_inv_exp_gcd() {
    let three = TeeBigInt::from_i32(3);
    let seven = TeeBigInt::from_i32(7);
    assert_eq!(three.inv_mod(&seven).unwrap().to_i32().unwrap(), 5);

    let two = TeeBigInt::from_i32(2);
    assert_eq!(
        two.exp_mod(&TeeBigInt::from_i32(3), &seven)
            .unwrap()
            .to_i32()
            .unwrap(),
        1
    );

    assert_eq!(
        TeeBigInt::from_i32(30)
            .gcd(&TeeBigInt::from_i32(18))
            .to_i32()
            .unwrap(),
        6
    );
    assert!(TeeBigInt::from_i32(15).relative_prime(&TeeBigInt::from_i32(28)));
}

#[test]
fn test_bigint_extended_gcd() {
    let a = TeeBigInt::from_i32(30);
    let b = TeeBigInt::from_i32(18);
    let (gcd, u, v) = a.extended_gcd(&b);
    assert_eq!(gcd.to_i32().unwrap(), 6);
    assert_eq!(
        u.mul(&a).add(&v.mul(&b)).to_i32().unwrap(),
        gcd.to_i32().unwrap()
    );
}

#[test]
fn test_bigint_probable_prime() {
    assert!(TeeBigInt::from_i32(97).is_probable_prime());
    assert!(!TeeBigInt::from_i32(98).is_probable_prime());
    assert!(TeeBigInt::from_i32(2).is_probable_prime());
    assert!(!TeeBigInt::from_i32(1).is_probable_prime());
    assert!(!TeeBigInt::from_i32(-97).is_probable_prime());
}

#[test]
fn test_bigint_bits() {
    let mut value = TeeBigInt::from_i32(5);
    assert!(value.get_bit(0));
    assert!(!value.get_bit(1));
    assert!(value.get_bit(2));
    value.set_bit(3, true);
    assert_eq!(value.to_i32().unwrap(), 13);
    assert_eq!(value.bit_length(), 4);
    assert_eq!(value.shr(2).to_i32().unwrap(), 3);
}
