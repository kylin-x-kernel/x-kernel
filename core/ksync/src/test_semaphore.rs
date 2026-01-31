//! Unit tests for Semaphore.

use unittest::mod_test;

#[mod_test]
mod semaphore_tests {
    use unittest::def_test;

    use crate::Semaphore;

    #[def_test]
    fn test_semaphore_basic_acquire_release() {
        // Test basic acquire and release operations
        let sem = Semaphore::new(2);

        assert_eq!(sem.available_permits(), 2);

        sem.acquire();
        assert_eq!(sem.available_permits(), 1);

        sem.acquire();
        assert_eq!(sem.available_permits(), 0);

        sem.release();
        assert_eq!(sem.available_permits(), 1);

        sem.release();
        assert_eq!(sem.available_permits(), 2);
    }

    #[def_test]
    fn test_semaphore_try_acquire_boundary() {
        // Test try_acquire at boundary conditions
        let sem = Semaphore::new(1);

        // First try_acquire should succeed
        assert!(sem.try_acquire());
        assert_eq!(sem.available_permits(), 0);

        // Second try_acquire should fail (no permits)
        assert!(!sem.try_acquire());
        assert_eq!(sem.available_permits(), 0);

        // After release, try_acquire should succeed again
        sem.release();
        assert_eq!(sem.available_permits(), 1);
        assert!(sem.try_acquire());
        assert_eq!(sem.available_permits(), 0);
    }

    #[def_test]
    fn test_semaphore_guard_raii() {
        // Test that SemaphoreGuard properly releases on drop
        let sem = Semaphore::new(3);

        assert_eq!(sem.available_permits(), 3);

        {
            let _guard1 = sem.acquire_guard();
            assert_eq!(sem.available_permits(), 2);

            {
                let _guard2 = sem.acquire_guard();
                assert_eq!(sem.available_permits(), 1);
            }

            // guard2 dropped, permit should be released
            assert_eq!(sem.available_permits(), 2);
        }

        // guard1 dropped, all permits should be back
        assert_eq!(sem.available_permits(), 3);
    }
}
