//! Unit tests for IoEvents.

#![allow(missing_docs)]

use unittest::{
    test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
};

use crate::IoEvents;

test_fn! {
    using TestResult;

    fn test_ioevents_bitflags_operations() {
        // Test basic bitflag operations
        let read = IoEvents::IN;
        let write = IoEvents::OUT;
        let read_write = read | write;

        assert!(read_write.contains(IoEvents::IN));
        assert!(read_write.contains(IoEvents::OUT));
        assert!(!read_write.contains(IoEvents::ERR));
    }
}

test_fn! {
    using TestResult;

    fn test_ioevents_always_poll() {
        // Test ALWAYS_POLL includes ERR and HUP
        let always = IoEvents::ALWAYS_POLL;

        assert!(always.contains(IoEvents::ERR));
        assert!(always.contains(IoEvents::HUP));
        assert!(!always.contains(IoEvents::IN));
        assert!(!always.contains(IoEvents::OUT));
    }
}

test_fn! {
    using TestResult;

    fn test_ioevents_intersection() {
        // Test intersection of event sets
        let events1 = IoEvents::IN | IoEvents::OUT | IoEvents::ERR;
        let events2 = IoEvents::OUT | IoEvents::HUP;

        let common = events1 & events2;

        assert!(common.contains(IoEvents::OUT));
        assert!(!common.contains(IoEvents::IN));
        assert!(!common.contains(IoEvents::ERR));
        assert!(!common.contains(IoEvents::HUP));
    }
}

tests_name!(TEST_IOEVENTS;
    test_ioevents_bitflags_operations,
    test_ioevents_always_poll,
    test_ioevents_intersection,
);
