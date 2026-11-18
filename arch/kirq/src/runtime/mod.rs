// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lifecycle implementation behind the crate's explicit public facade.
//!
//! Entry owns trap context and tail ordering; manager owns control transactions;
//! state owns Line invariants and registry serialization. Dispatch implements
//! admitted flow execution, while action storage and NMI publication retain
//! their own reclamation rules. Index RCU protects directory storage, not
//! hardware quiescence or ordinary action lifetime.

pub(crate) mod action;
mod action_set;
pub(crate) mod affinity;
pub(crate) mod dispatch;
pub(crate) mod entry;
pub(crate) mod handle;
pub(crate) mod index;
pub(crate) mod manager;
pub(crate) mod nmi;
pub(crate) mod rcu;
pub mod resend;
pub(crate) mod retirement;
pub(crate) mod state;
mod unhandled;
