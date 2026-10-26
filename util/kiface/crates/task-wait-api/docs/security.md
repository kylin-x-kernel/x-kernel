# Task Wait API Security

The provider must validate non-idle initialized current, IRQ/preemption state,
IRQ/BH exclusion and active RCU reads before invoking the body. Rejection leaves
the body uncalled even when completion is already signaled. Success invokes it
once; returning an error afterward would misreport already-published work.

Prepared wait storage and its strong task reference outlive all stack wait
registrations. The borrow cannot escape or move tasks; the preparing task may
migrate normally. Waiting is uninterruptible and allocation-free. Producers
must publish state before notification; consumers must not retain locks needed
by the producer and must recheck their own completion predicates.

The crate contains no unsafe blocks or concrete runtime state. The compile-fail
doctest checks that the scoped wait cannot escape. ktask provider tests check
rejection-before-body, repeated broadcast acquisition and actual blocking.

PreparedParking has a distinct contract: a cloned waker can outlive the scope
and parking may return spuriously. It is a hint channel, not completion. Its
provider retains the original allocation and task binding through the body;
preparation may use fatal allocation, while park/waker cloning do not allocate.
