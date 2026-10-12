use std::sync::Arc;

use flatten_objects::FlattenObjects;

mod sealed {
    use core::fmt;

    const INNER: u8 = 0x1f;

    pub struct Object(u8);

    impl Default for Object {
        fn default() -> Self {
            Self(INNER)
        }
    }

    impl fmt::Debug for Object {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "Object")
        }
    }

    impl Clone for Object {
        fn clone(&self) -> Self {
            assert_eq!(self.0, INNER, "Object::clone");
            Self(self.0)
        }
    }

    impl Drop for Object {
        fn drop(&mut self) {
            assert_eq!(self.0, INNER, "Object::drop");
        }
    }
}

pub use sealed::Object;

#[test]
fn test_object() {
    let mut objects = FlattenObjects::<Object, 32>::new();

    objects.add(Object::default()).unwrap();
    objects.add_at(10, Object::default()).unwrap();

    let mut cloned = objects.clone();
    cloned.remove(0).unwrap();
}

#[test]
fn test_arc() {
    let src = Arc::new(());

    let mut objects = FlattenObjects::<Arc<()>, 32>::new();

    objects.add(src.clone()).unwrap();
    objects.add_at(10, src.clone()).unwrap();
    assert_eq!(Arc::strong_count(&src), 3);

    let mut cloned = objects.clone();
    assert_eq!(Arc::strong_count(&src), 5);

    cloned.remove(0).unwrap();
    assert_eq!(Arc::strong_count(&src), 4);

    drop(cloned);
    assert_eq!(Arc::strong_count(&src), 3);

    drop(objects);
    assert_eq!(Arc::strong_count(&src), 1);
}
