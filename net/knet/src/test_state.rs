//! Unit tests for socket state management.

#![cfg(unittest)]

use unittest::def_test;

use crate::state::{State, StateLock};

#[def_test]
fn test_state_enum_values() {
    // Test all state enum values can be created
    let states = [
        State::Idle,
        State::Busy,
        State::Connecting,
        State::Connected,
        State::Listening,
        State::Closed,
    ];

    // Verify each state can be compared
    assert_ne!(State::Idle, State::Busy);
    assert_ne!(State::Connected, State::Connecting);
    assert_ne!(State::Listening, State::Closed);

    // Test state ordering in transitions
    assert_ne!(states[0], states[1]);
    assert_ne!(states[1], states[2]);
    assert_ne!(states[2], states[3]);
    assert_ne!(states[3], states[4]);
    assert_ne!(states[4], states[5]);
}

#[def_test]
fn test_state_try_from_valid() {
    // Test valid conversions from u8 to State
    assert_eq!(State::try_from(0), Ok(State::Idle));
    assert_eq!(State::try_from(1), Ok(State::Busy));
    assert_eq!(State::try_from(2), Ok(State::Connecting));
    assert_eq!(State::try_from(3), Ok(State::Connected));
    assert_eq!(State::try_from(4), Ok(State::Listening));
    assert_eq!(State::try_from(5), Ok(State::Closed));
}

#[def_test]
fn test_state_try_from_invalid() {
    // Test invalid conversions return Err
    assert!(State::try_from(6).is_err());
    assert!(State::try_from(255).is_err());
    assert!(State::try_from(100).is_err());

    // Test boundary values
    assert!(State::try_from(u8::MAX).is_err());
    assert!(State::try_from(7).is_err());
}

#[def_test]
fn test_state_lock_creation_and_get() {
    // Test creating StateLock with different initial states
    let idle_lock = StateLock::new(State::Idle);
    assert_eq!(idle_lock.get(), State::Idle);

    let connected_lock = StateLock::new(State::Connected);
    assert_eq!(connected_lock.get(), State::Connected);

    let closed_lock = StateLock::new(State::Closed);
    assert_eq!(closed_lock.get(), State::Closed);
}

#[def_test]
fn test_state_lock_set() {
    // Test state transitions via set
    let lock = StateLock::new(State::Idle);
    assert_eq!(lock.get(), State::Idle);

    lock.set(State::Connecting);
    assert_eq!(lock.get(), State::Connecting);

    lock.set(State::Connected);
    assert_eq!(lock.get(), State::Connected);

    lock.set(State::Closed);
    assert_eq!(lock.get(), State::Closed);
}

#[def_test]
fn test_state_lock_successful() {
    // Test successful lock acquisition
    let lock = StateLock::new(State::Idle);

    // Lock should succeed when state matches
    let guard = lock.lock(State::Idle);
    assert!(guard.is_ok());

    // After acquiring lock, state should be Busy
    assert_eq!(lock.get(), State::Busy);
}

#[def_test]
fn test_state_lock_failure() {
    // Test lock failure when state doesn't match
    let lock = StateLock::new(State::Connected);

    // Try to lock with wrong expected state
    let result = lock.lock(State::Idle);
    assert!(result.is_err());

    // State should remain unchanged
    assert_eq!(lock.get(), State::Connected);

    // Verify the error contains the actual state
    match result {
        Err(state) => assert_eq!(state, State::Connected),
        Ok(_) => panic!("Expected lock to fail"),
    }
}

#[def_test]
fn test_state_guard_transit_success() {
    // Test successful state transition
    let lock = StateLock::new(State::Idle);
    let guard = lock.lock(State::Idle).unwrap();

    let result = guard.transit(State::Connected, || Ok(42));
    assert_eq!(result.unwrap(), 42);
    assert_eq!(lock.get(), State::Connected);
}

#[def_test]
fn test_state_guard_transit_failure() {
    // Test state rollback on error
    let lock = StateLock::new(State::Connecting);
    let guard = lock.lock(State::Connecting).unwrap();

    let result: Result<(), _> =
        guard.transit(State::Connected, || Err(kerrno::KError::ConnectionRefused));
    assert!(result.is_err());

    // State should rollback to Connecting (the original state)
    assert_eq!(lock.get(), State::Connecting);
}

#[def_test]
fn test_state_lock_concurrent_semantics() {
    // Test that lock properly enforces state-based mutual exclusion
    let lock = StateLock::new(State::Idle);

    // First lock succeeds
    let guard1 = lock.lock(State::Idle);
    assert!(guard1.is_ok());
    assert_eq!(lock.get(), State::Busy);

    // Second lock with Idle should fail (state is now Busy)
    let guard2 = lock.lock(State::Idle);
    assert!(guard2.is_err());
    match guard2 {
        Err(state) => assert_eq!(state, State::Busy),
        Ok(_) => panic!("Expected lock to fail"),
    }

    // Trying to lock with Busy succeeds (state matches)
    // This allows re-locking with the current state
    let guard3 = lock.lock(State::Busy);
    assert!(guard3.is_ok());

    // After guard3 takes the lock, state remains Busy
    assert_eq!(lock.get(), State::Busy);
}

#[def_test]
fn test_state_transition_sequence() {
    // Test a realistic sequence of state transitions
    let lock = StateLock::new(State::Idle);

    // Idle -> Connecting
    {
        let guard = lock.lock(State::Idle).unwrap();
        guard.transit(State::Connecting, || Ok(())).unwrap();
    }
    assert_eq!(lock.get(), State::Connecting);

    // Connecting -> Connected
    {
        let guard = lock.lock(State::Connecting).unwrap();
        guard.transit(State::Connected, || Ok(())).unwrap();
    }
    assert_eq!(lock.get(), State::Connected);

    // Connected -> Closed
    {
        let guard = lock.lock(State::Connected).unwrap();
        guard.transit(State::Closed, || Ok(())).unwrap();
    }
    assert_eq!(lock.get(), State::Closed);
}

#[def_test]
fn test_state_listening_transitions() {
    // Test listening state specific transitions
    let lock = StateLock::new(State::Idle);

    // Idle -> Listening
    {
        let guard = lock.lock(State::Idle).unwrap();
        guard.transit(State::Listening, || Ok(())).unwrap();
    }
    assert_eq!(lock.get(), State::Listening);

    // Listening -> Closed
    {
        let guard = lock.lock(State::Listening).unwrap();
        guard.transit(State::Closed, || Ok(())).unwrap();
    }
    assert_eq!(lock.get(), State::Closed);
}
