extern crate alloc;

use alloc::string::{String, ToString};

use hashbrown::HashMap;
use uuid as uuid_crate;

/// Identity of a TA session, stored in `TeeTaCtx.open_sessions`.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub uuid: String,
    pub session_id: u32,
}

/// Global TA context shared across all sessions of one TA instance.
#[derive(Debug)]
pub struct TeeTaCtx {
    pub session_dispatch_irq: u32,
    pub open_sessions: HashMap<u32, SessionIdentity>,
    pub uuid: String,
}

impl Default for TeeTaCtx {
    fn default() -> Self {
        TeeTaCtx {
            session_dispatch_irq: 0,
            open_sessions: HashMap::new(),
            uuid: uuid_crate::Uuid::default().to_string(),
        }
    }
}

impl TeeTaCtx {
    pub fn set_uuid(&mut self, path: &str) {
        // get the path basic string
        let uuid = match path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.').map(|(base, _)| base))
        {
            Some(v) => v,
            None => return,
        };

        if uuid_crate::Uuid::parse_str(uuid).is_ok() {
            self.uuid = uuid.to_string();
        }
    }

    pub fn new(path: &str) -> Self {
        let mut ctx = Self::default();
        ctx.set_uuid(path);
        ctx
    }
}

// Test module for TEE session functionality
// Only compiled when the tee_test feature is enabled
#[unittest::mod_test]
pub mod tests_ta_ctx {
    use unittest::{assert, assert_eq};

    use super::*;

    // Test function for basic ta_ctx operations
    #[unittest::def_test]
    fn test_ta_ctx() {
        let mut ta_ctx = TeeTaCtx::default();
        ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8.ta");
        assert_eq!(ta_ctx.uuid, "936da01f-9abd-4d9d-80c7-02af85c822a8");
        ta_ctx.uuid.clear();
        assert!(ta_ctx.uuid.is_empty());
        ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a.ta");
        assert!(ta_ctx.uuid.is_empty());
        ta_ctx.set_uuid("/tee/ta/936da01f-9abd-4d9d-80c7-02af85c822a8");
        assert!(ta_ctx.uuid.is_empty());
        ta_ctx.set_uuid("/tee/ta/936DA01F-9ABD-4D9D-80C7-02AF85C822A8.ta");
        assert_eq!(ta_ctx.uuid, "936DA01F-9ABD-4D9D-80C7-02AF85C822A8");
    }
}
