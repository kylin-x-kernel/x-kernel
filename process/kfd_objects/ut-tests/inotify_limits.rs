// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

/// A ledger isolated from the global one, so limit changes in these tests
/// cannot affect the live sysctl values other tests read.
fn ledger(max_watches: usize, max_instances: usize, max_events: usize) -> Arc<Resources> {
    Arc::new(Resources::new(max_watches, max_instances, max_events))
}

#[def_test]
fn sysctl_write_validates_before_mutating() {
    let resources = ledger(10, 20, 30);

    // Value checks run under the privileged kernel credential. Refusal is
    // exercised separately with a writer whose euid is not 0.
    assert_eq!(
        resources.write(InotifyLimit::UserWatches, b"not-a-number", &kcred::initial_cred()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        resources.write(InotifyLimit::UserWatches, &[0xff, 0xfe], &kcred::initial_cred()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        resources.write(
            InotifyLimit::UserWatches,
            alloc::format!("{}", MAX_LIMIT + 1).as_bytes(),
            &kcred::initial_cred()
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(resources.value(InotifyLimit::UserWatches), 10);

    // A valid write applies, including the trailing newline a sysctl write has.
    assert_eq!(
        resources.write(InotifyLimit::UserWatches, b"64\n", &kcred::initial_cred()),
        Ok(())
    );
    assert_eq!(resources.value(InotifyLimit::UserWatches), 64);
}

#[def_test]
fn sysctl_write_requires_the_writing_task_to_be_privileged() {
    let resources = ledger(10, 20, 30);

    // Linux checks the writing task's current euid, not the uid that opened
    // the descriptor. A DAC-root credential (fsuid 0, euid 1000) must not be
    // able to change a system-wide limit.
    let mut dac_root = kcred::Cred::new(0, 0);
    dac_root.set_reuid(None, Some(1000)).unwrap();
    dac_root.set_fsuid(0);
    assert_eq!((dac_root.euid(), dac_root.fsuid()), (1000, 0));
    assert_eq!(
        resources.write(InotifyLimit::UserWatches, b"64", &dac_root),
        Err(KError::OperationNotPermitted)
    );
    assert_eq!(resources.value(InotifyLimit::UserWatches), 10);

    // Conversely, an effective-root writer applies the value even when its
    // filesystem identity is non-root.
    let mut privileged = kcred::Cred::new(0, 0);
    privileged.set_fsuid(1000);
    assert_eq!((privileged.euid(), privileged.fsuid()), (0, 1000));
    assert_eq!(
        resources.write(InotifyLimit::UserWatches, b"64", &privileged),
        Ok(())
    );
    assert_eq!(resources.value(InotifyLimit::UserWatches), 64);
}

#[def_test]
fn reservations_are_per_user_and_returned_on_drop() {
    let resources = ledger(2, 1, 8);
    let first_uid = 1001;
    let second_uid = 1002;

    let instance = resources.reserve(first_uid, Resource::Instance).unwrap();
    let watch_a = resources.reserve(first_uid, Resource::Watch).unwrap();
    let watch_b = resources.reserve(first_uid, Resource::Watch).unwrap();

    // The watch budget is per user: uid 1001 is at its limit, 1002 is not.
    assert_eq!(
        resources.reserve(first_uid, Resource::Watch).err(),
        Some(KError::StorageFull)
    );
    let other_watch = resources.reserve(second_uid, Resource::Watch).unwrap();

    // The instance budget is per user as well.
    assert_eq!(
        resources.reserve(first_uid, Resource::Instance).err(),
        Some(KError::TooManyOpenFiles)
    );
    let other_instance = resources.reserve(second_uid, Resource::Instance).unwrap();

    // Dropping returns capacity, and a fully drained user leaves no entry.
    drop(watch_b);
    let reacquired = resources.reserve(first_uid, Resource::Watch).unwrap();
    drop(reacquired);
    drop(watch_a);
    drop(instance);
    let ledger = resources.ledger.lock();
    assert!(!ledger.users.contains_key(&first_uid));
    // The other user's live reservations remain visible.
    assert_eq!(ledger.users.get(&second_uid).map(|u| u.watches), Some(1));
    drop(ledger);
    drop(other_watch);
    drop(other_instance);
}

#[def_test]
fn defaults_match_the_published_constants() {
    assert_eq!(
        InotifyLimit::UserWatches.value(),
        DEFAULT_MAX_USER_WATCHES
    );
    assert_eq!(
        InotifyLimit::UserInstances.value(),
        DEFAULT_MAX_USER_INSTANCES
    );
    assert_eq!(
        InotifyLimit::QueuedEvents.value(),
        DEFAULT_MAX_QUEUED_EVENTS
    );
}
#[def_test]
fn sysctl_write_follows_proc_get_long_rules() {
    // The first whitespace-terminated token decides the value; the rest is the
    // next vector element's business, so a short write, extra values and even
    // a non-UTF-8 tail apply the first number instead of failing.
    let resources = ledger(1, 1, 16);
    let cred = kcred::initial_cred();
    let write = |data: &[u8]| resources.write(InotifyLimit::QueuedEvents, data, &cred);

    assert_eq!(write(b"8192 0"), Ok(()));
    assert_eq!(resources.max_events(), 8192);
    assert_eq!(write(b"8192 \xff"), Ok(()));
    assert_eq!(resources.max_events(), 8192);
    assert_eq!(write(b"  12\n"), Ok(()));
    assert_eq!(resources.max_events(), 12);

    // Base 0, as `_parse_integer_fixup_radix` selects it: hex and octal are
    // values, not errors, and the leading `0` is a base marker.
    assert_eq!(write(b"0x10"), Ok(()));
    assert_eq!(resources.max_events(), 16);
    assert_eq!(write(b"0X1f"), Ok(()));
    assert_eq!(resources.max_events(), 31);
    assert_eq!(write(b"010"), Ok(()));
    assert_eq!(resources.max_events(), 8);

    // Rejected exactly where `proc_get_long` rejects: a sign, garbage inside
    // the token, and an octal digit out of range.
    assert_eq!(write(b"+5"), Err(KError::InvalidInput));
    assert_eq!(write(b"8192abc"), Err(KError::InvalidInput));
    assert_eq!(write(b"0000008192"), Err(KError::InvalidInput));
    assert_eq!(write(b""), Err(KError::InvalidInput));

    // `proc_get_long` copies at most TMPBUFLEN-1 (21) bytes, so a token that
    // long is rejected instead of applied; 20 padded digits still parse.
    let twenty = b"00000000000000000001"; // octal 1
    assert_eq!(twenty.len(), 20);
    assert_eq!(write(twenty), Ok(()));
    assert_eq!(resources.max_events(), 1);
    let twenty_one = b"000000000000000000021"; // 21 chars
    assert_eq!(twenty_one.len(), 21);
    assert_eq!(write(twenty_one), Err(KError::InvalidInput));
    assert_eq!(resources.max_events(), 1);
}
