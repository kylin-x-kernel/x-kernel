// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Line-serialized membership with scoped RCU traversal and publication ceilings.

use alloc::sync::Arc;

use kiface_irq_context_api::InterruptContextIf;
use krcu_list::{PreparedListTake, RcuList, RetiredListEntry};

use super::action::{IrqAction, IrqActionToken, PreparedAction};

pub(super) struct ActionRecord {
    pub(super) action: IrqAction,
    pub(super) sequence: usize,
}

impl ActionRecord {
    pub(super) fn into_action(self) -> IrqAction {
        self.action
    }
}

pub(super) struct IrqActionSnapshot {
    list: Arc<RcuList<ActionRecord>>,
    ceiling: usize,
}

impl IrqActionSnapshot {
    pub(super) fn with_actions(&self, mut callback: impl FnMut(&IrqAction)) {
        let visit = |scope: &krcu::ReadScope<'_>| {
            for record in self.list.iter(scope) {
                if record.sequence > self.ceiling {
                    break;
                }
                callback(&record.action);
            }
        };
        if InterruptContextIf::is_in_interrupt_handler() {
            krcu::with_interrupt_read(visit);
        } else {
            krcu::with_read(visit);
        }
    }
}

pub(super) struct ActionSet {
    list: Arc<RcuList<ActionRecord>>,
    count: usize,
    has_shared: bool,
    sequence: usize,
}

impl ActionSet {
    pub(super) fn new() -> Self {
        Self {
            list: Arc::new(RcuList::new()),
            count: 0,
            has_shared: false,
            sequence: 0,
        }
    }

    pub(super) const fn len(&self) -> usize {
        self.count
    }

    pub(super) fn can_install_regular(&self) -> bool {
        self.count == 0 && self.sequence.checked_add(1).is_some()
    }

    pub(super) fn can_install_shared(&self) -> bool {
        (self.count == 0 || self.has_shared)
            && self.sequence.checked_add(1).is_some()
            && self.count.checked_add(1).is_some()
    }

    pub(super) fn publish_regular(&mut self, prepared: &mut PreparedAction) -> bool {
        if prepared.is_shared() || !self.can_install_regular() {
            return false;
        }
        self.publish(prepared).is_some()
    }

    pub(super) fn publish_shared(
        &mut self,
        prepared: &mut PreparedAction,
    ) -> Option<IrqActionToken> {
        if !prepared.is_shared() || !self.can_install_shared() {
            return None;
        }
        self.publish(prepared).map(IrqActionToken::new)
    }

    fn publish(&mut self, prepared: &mut PreparedAction) -> Option<usize> {
        let sequence = self.sequence.checked_add(1)?;
        let count = self.count.checked_add(1)?;
        let mut entry = prepared.pending.take()?;
        let record = entry.get_mut();
        record.sequence = sequence;
        if prepared.is_shared() {
            record
                .action
                .set_shared_token(IrqActionToken::new(sequence));
        }
        // SAFETY: callers serialize membership under the Line lock in task
        // context; entry allocation preceded exclusion and no writer is reentered.
        unsafe { self.list.push_back(entry) };
        self.sequence = sequence;
        self.count = count;
        self.has_shared = prepared.is_shared();
        Some(sequence)
    }

    pub(super) fn list(&self) -> Arc<RcuList<ActionRecord>> {
        self.list.clone()
    }

    /// Preparation comes from this retained Line's action_list. Caller retains
    /// Line exclusion through unlink, then reclaims outside short locks.
    pub(super) fn withdraw<'wait>(
        &mut self,
        prepared: PreparedListTake<'_, 'wait, ActionRecord>,
        token: Option<IrqActionToken>,
    ) -> Option<RetiredListEntry<'wait, ActionRecord>> {
        if self.count == 0 || self.has_shared != token.is_some() {
            return None;
        }
        // SAFETY: caller is the preparing task under Line exclusion. The
        // predicate only compares immutable keys and does not reenter writers.
        let retired = unsafe {
            prepared.withdraw_first(|record| match token {
                Some(token) => record.action.is_shared() && record.action.token() == token,
                None => !record.action.is_shared(),
            })
        };
        if !retired.is_some() {
            return None;
        }
        self.count -= 1;
        if self.count == 0 {
            self.has_shared = false;
        }
        Some(retired)
    }

    pub(super) fn snapshot(&self) -> IrqActionSnapshot {
        IrqActionSnapshot {
            list: self.list.clone(),
            ceiling: self.sequence,
        }
    }

    #[cfg(unittest)]
    pub(super) fn exhaust_sequences(&mut self) {
        self.sequence = usize::MAX;
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/action_set.rs");
