//! Unit tests for socket options.

#![cfg(unittest)]

extern crate alloc;
use core::time::Duration;

use unittest::def_test;

use crate::options::{GetSocketOption, SetSocketOption, UnixCredentials};

#[def_test]
fn test_unix_credentials_construction() {
    // Test default construction
    let default_cred = UnixCredentials::default();
    assert_eq!(default_cred.pid, 0);
    assert_eq!(default_cred.uid, 0);
    assert_eq!(default_cred.gid, 0);

    // Test with specific PID
    let cred = UnixCredentials::new(1234);
    assert_eq!(cred.pid, 1234);
    assert_eq!(cred.uid, 0);
    assert_eq!(cred.gid, 0);

    // Test boundary values
    let max_pid_cred = UnixCredentials::new(u32::MAX);
    assert_eq!(max_pid_cred.pid, u32::MAX);

    let min_pid_cred = UnixCredentials::new(u32::MIN);
    assert_eq!(min_pid_cred.pid, 0);
}

#[def_test]
fn test_unix_credentials_clone() {
    // Test clone functionality
    let original = UnixCredentials {
        pid: 100,
        uid: 1000,
        gid: 2000,
    };

    let cloned = original.clone();
    assert_eq!(original.pid, cloned.pid);
    assert_eq!(original.uid, cloned.uid);
    assert_eq!(original.gid, cloned.gid);

    // Verify modifications to clone don't affect original
    let mut modified_clone = original.clone();
    modified_clone.pid = 999;
    assert_eq!(original.pid, 100);
    assert_eq!(modified_clone.pid, 999);
}

#[def_test]
fn test_socket_option_variants_get() {
    // Test GetSocketOption variants can be constructed
    let mut reuse_addr = false;
    let _get_opt = GetSocketOption::ReuseAddress(&mut reuse_addr);

    let mut error = 0i32;
    let _get_opt = GetSocketOption::Error(&mut error);

    let mut send_buf = 0usize;
    let _get_opt = GetSocketOption::SendBuffer(&mut send_buf);

    let mut recv_buf = 0usize;
    let _get_opt = GetSocketOption::ReceiveBuffer(&mut recv_buf);

    let mut keep_alive = false;
    let _get_opt = GetSocketOption::KeepAlive(&mut keep_alive);

    let mut ttl = 0u8;
    let _get_opt = GetSocketOption::Ttl(&mut ttl);

    let mut non_blocking = false;
    let _get_opt = GetSocketOption::NonBlocking(&mut non_blocking);

    // Verify the variables are mutable
    reuse_addr = true;
    assert_eq!(reuse_addr, true);
}

#[def_test]
fn test_socket_option_variants_set() {
    // Test SetSocketOption variants can be constructed and copied
    let reuse_addr = true;
    let set_opt1 = SetSocketOption::ReuseAddress(&reuse_addr);
    let set_opt2 = set_opt1; // Test Copy trait

    // Both should be valid
    match set_opt1 {
        SetSocketOption::ReuseAddress(val) => assert!(*val),
        _ => panic!("Expected ReuseAddress variant"),
    }
    match set_opt2 {
        SetSocketOption::ReuseAddress(val) => assert!(*val),
        _ => panic!("Expected ReuseAddress variant"),
    }

    // Test timeout options
    let send_timeout = Duration::from_secs(5);
    let set_opt = SetSocketOption::SendTimeout(&send_timeout);
    match set_opt {
        SetSocketOption::SendTimeout(d) => assert_eq!(*d, Duration::from_secs(5)),
        _ => panic!("Expected SendTimeout variant"),
    }

    let recv_timeout = Duration::from_millis(100);
    let set_opt = SetSocketOption::ReceiveTimeout(&recv_timeout);
    match set_opt {
        SetSocketOption::ReceiveTimeout(d) => assert_eq!(*d, Duration::from_millis(100)),
        _ => panic!("Expected ReceiveTimeout variant"),
    }

    // Test boundary values for buffer sizes
    let max_buf = usize::MAX;
    let set_opt = SetSocketOption::SendBuffer(&max_buf);
    match set_opt {
        SetSocketOption::SendBuffer(size) => assert_eq!(*size, usize::MAX),
        _ => panic!("Expected SendBuffer variant"),
    }

    let zero_buf = 0usize;
    let set_opt = SetSocketOption::ReceiveBuffer(&zero_buf);
    match set_opt {
        SetSocketOption::ReceiveBuffer(size) => assert_eq!(*size, 0),
        _ => panic!("Expected ReceiveBuffer variant"),
    }
}

#[def_test]
fn test_timeout_boundary_values() {
    // Test zero timeout
    let zero_timeout = Duration::from_secs(0);
    let set_opt = SetSocketOption::SendTimeout(&zero_timeout);
    match set_opt {
        SetSocketOption::SendTimeout(d) => {
            assert_eq!(d.as_secs(), 0);
            assert_eq!(d.as_nanos(), 0);
        }
        _ => panic!("Expected SendTimeout variant"),
    }

    // Test very large timeout
    let large_timeout = Duration::from_secs(u64::MAX);
    let set_opt = SetSocketOption::ReceiveTimeout(&large_timeout);
    match set_opt {
        SetSocketOption::ReceiveTimeout(d) => assert_eq!(d.as_secs(), u64::MAX),
        _ => panic!("Expected ReceiveTimeout variant"),
    }

    // Test sub-second timeout
    let micro_timeout = Duration::from_micros(1);
    let set_opt = SetSocketOption::SendTimeout(&micro_timeout);
    match set_opt {
        SetSocketOption::SendTimeout(d) => {
            assert_eq!(d.as_secs(), 0);
            assert!(d.as_nanos() > 0);
        }
        _ => panic!("Expected SendTimeout variant"),
    }
}

#[def_test]
fn test_ttl_boundary_values() {
    // Test minimum TTL
    let min_ttl = 0u8;
    let set_opt = SetSocketOption::Ttl(&min_ttl);
    match set_opt {
        SetSocketOption::Ttl(ttl) => assert_eq!(*ttl, 0),
        _ => panic!("Expected Ttl variant"),
    }

    // Test maximum TTL
    let max_ttl = 255u8;
    let set_opt = SetSocketOption::Ttl(&max_ttl);
    match set_opt {
        SetSocketOption::Ttl(ttl) => assert_eq!(*ttl, 255),
        _ => panic!("Expected Ttl variant"),
    }

    // Test typical default TTL (64)
    let default_ttl = 64u8;
    let set_opt = SetSocketOption::Ttl(&default_ttl);
    match set_opt {
        SetSocketOption::Ttl(ttl) => assert_eq!(*ttl, 64),
        _ => panic!("Expected Ttl variant"),
    }
}
