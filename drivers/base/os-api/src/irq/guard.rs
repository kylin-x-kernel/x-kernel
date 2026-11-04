// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ownership of one IRQ registration and its exact provider release identity.

use alloc::sync::Arc;
use core::fmt;

use super::{IrqHandler, IrqHandlerToken, IrqOp, IrqResource, IrqThreadHandler, Virq};
use crate::ResResult;

/// One registered device interrupt action, synchronously released on Drop.
///
/// Stop device access first and drop in sleepable task context without locks
/// needed by callbacks. Owning an action does not imply ownership of the line.
/// Provider-backed guards retain the exact resource/token until release.
pub struct DevIrq {
    provider: &'static dyn IrqOp,
    resource: IrqResource,
    token: IrqHandlerToken,
}

impl DevIrq {
    /// Register through a static provider and retain the exact release identity.
    pub fn request_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        handler: Arc<dyn IrqHandler>,
    ) -> ResResult<Self> {
        let token = provider.request_irq(resource, handler)?;
        Ok(Self {
            provider,
            resource,
            token,
        })
    }

    /// Register a primary and threaded handler through a static provider.
    ///
    /// Providers without threaded support return Unsupported without a guard.
    pub fn request_threaded_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        primary: Arc<dyn IrqHandler>,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<Self> {
        let token = provider.request_threaded_irq(resource, primary, thread, name)?;
        Ok(Self {
            provider,
            resource,
            token,
        })
    }

    /// Register a threaded handler with the provider's default primary.
    pub fn request_threaded_default_with(
        provider: &'static dyn IrqOp,
        resource: IrqResource,
        thread: Arc<dyn IrqThreadHandler>,
        name: Option<&'static str>,
    ) -> ResResult<Self> {
        let token = provider.request_threaded_irq_default(resource, thread, name)?;
        Ok(Self {
            provider,
            resource,
            token,
        })
    }

    /// The OS-visible IRQ for this registration.
    pub fn virq(&self) -> Virq {
        self.resource.virq()
    }
}

impl Drop for DevIrq {
    fn drop(&mut self) {
        self.provider.release_irq(self.resource, self.token);
    }
}

impl fmt::Debug for DevIrq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DevIrq")
            .field("virq", &self.virq())
            .finish_non_exhaustive()
    }
}
