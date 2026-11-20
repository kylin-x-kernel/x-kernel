// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal type erasure for the object-safe driver registry.

use alloc::sync::Arc;

use driver_base::{DeviceKind, DriverError, DriverResult};

use super::{
    BusTypeId, DeviceDriver, DeviceMatcher,
    context::{PmContext, ProbeContext, RemoveContext},
    data::DriverData,
};

/// Failure from an erased callback.
#[derive(Debug)]
pub(crate) enum ErasedCallError {
    /// The driver ran and returned a normal (post-stop) error.
    Driver(DriverError),
    /// Internal data type mismatch: the callback was never invoked.
    TypeMismatch,
}

/// Object-safe view of one typed driver.
pub(crate) trait ErasedDeviceDriver: Send + Sync {
    fn supports_suspend(&self) -> bool;
    fn name(&self) -> &'static str;
    fn device_kind(&self) -> DeviceKind;
    fn bus_types(&self) -> &'static [BusTypeId];
    fn matcher(&self) -> &dyn DeviceMatcher;

    fn probe(&self, ctx: &ProbeContext<'_>) -> DriverResult<DriverData>;
    fn remove(
        &self,
        ctx: &RemoveContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError>;
    fn suspend(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError>;
    fn resume(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError>;
    fn shutdown(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError>;
}

impl<D: DeviceDriver + 'static> ErasedDeviceDriver for D {
    fn supports_suspend(&self) -> bool {
        DeviceDriver::supports_suspend(self)
    }

    fn name(&self) -> &'static str {
        DeviceDriver::name(self)
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceDriver::device_kind(self)
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        DeviceDriver::bus_types(self)
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        DeviceDriver::matcher(self)
    }

    fn probe(&self, ctx: &ProbeContext<'_>) -> DriverResult<DriverData> {
        Ok(DriverData::new(self.probe_device(ctx)?))
    }

    fn remove(
        &self,
        ctx: &RemoveContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError> {
        let Some(data) = data else {
            return DeviceDriver::remove(self, ctx, None).map_err(ErasedCallError::Driver);
        };
        let Some(data) = data.downcast_mut::<D::Data>() else {
            return Err(ErasedCallError::TypeMismatch);
        };
        DeviceDriver::remove(self, ctx, Some(data)).map_err(ErasedCallError::Driver)
    }

    fn suspend(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError> {
        let Some(data) = data else {
            return DeviceDriver::suspend(self, ctx, None).map_err(ErasedCallError::Driver);
        };
        let Some(data) = data.downcast_mut::<D::Data>() else {
            return Err(ErasedCallError::TypeMismatch);
        };
        DeviceDriver::suspend(self, ctx, Some(data)).map_err(ErasedCallError::Driver)
    }

    fn resume(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError> {
        let Some(data) = data else {
            return DeviceDriver::resume(self, ctx, None).map_err(ErasedCallError::Driver);
        };
        let Some(data) = data.downcast_mut::<D::Data>() else {
            return Err(ErasedCallError::TypeMismatch);
        };
        DeviceDriver::resume(self, ctx, Some(data)).map_err(ErasedCallError::Driver)
    }

    fn shutdown(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError> {
        let Some(data) = data else {
            DeviceDriver::shutdown(self, ctx, None);
            return Ok(());
        };
        let Some(data) = data.downcast_mut::<D::Data>() else {
            return Err(ErasedCallError::TypeMismatch);
        };
        DeviceDriver::shutdown(self, ctx, Some(data));
        Ok(())
    }
}

/// Public opaque driver handle. Type erasure stays inside kdevice.
#[derive(Clone)]
pub struct BoxedDriver {
    ops: Arc<dyn ErasedDeviceDriver>,
    adoption: bool,
}

impl BoxedDriver {
    pub fn new<D: DeviceDriver + 'static>(driver: D) -> Self {
        Self {
            ops: Arc::new(driver),
            adoption: false,
        }
    }

    /// Register a driver that supports the legacy no-Data adoption path.
    /// The associated type bound is what guarantees `Data=()`.
    ///
    /// ```compile_fail
    /// use kdevice::{DeviceDriver, DeviceMatcher, DriverResult, ProbeContext};
    ///
    /// struct DataDriver;
    /// static MATCHER: kdevice::NeverMatcher = kdevice::NeverMatcher;
    /// impl DeviceDriver for DataDriver {
    ///     type Data = u8;
    ///     fn name(&self) -> &'static str { "data" }
    ///     fn device_kind(&self) -> kdevice::DeviceKind { kdevice::DeviceKind::Char }
    ///     fn bus_types(&self) -> &'static [kdevice::BusTypeId] { &[] }
    ///     fn matcher(&self) -> &dyn DeviceMatcher { &MATCHER }
    ///     fn probe_device(&self, _: &ProbeContext<'_>) -> DriverResult<u8> { Ok(0) }
    /// }
    /// let _ = kdevice::BoxedDriver::new_adoptable(DataDriver);
    /// ```
    pub fn new_adoptable<D: DeviceDriver<Data = ()> + 'static>(driver: D) -> Self {
        Self {
            ops: Arc::new(driver),
            adoption: true,
        }
    }

    pub fn supports_adoption(&self) -> bool {
        self.adoption
    }

    /// Matcher metadata for tests and registry adapters.
    pub fn matcher(&self) -> &dyn DeviceMatcher {
        self.ops.matcher()
    }

    pub(crate) fn erased(&self) -> &dyn ErasedDeviceDriver {
        self.ops.as_ref()
    }
}

impl<D: DeviceDriver + 'static> From<Arc<D>> for BoxedDriver {
    fn from(driver: Arc<D>) -> Self {
        Self {
            ops: driver,
            adoption: false,
        }
    }
}
