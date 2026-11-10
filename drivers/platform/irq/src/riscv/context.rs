// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Controller-owned PLIC context identity storage.
//!
//! Firmware supplies declarations with raw context indices; this module maps
//! them to running logical CPUs, validates them against the register layout and
//! owns the immutable lookup table used by preparation, enable, claim and
//! completion. It does not define firmware properties or register geometry.

use kcpu_id_map::LogicalCpuId;

use super::registers::PlicRegisters;

/// One firmware-declared S-mode context, before logical-CPU association.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlicContextDecl {
    /// Raw `interrupts-extended` index.
    pub(super) index: usize,
    /// Firmware-decoded architecture hardware CPU identity.
    pub(super) hardware_cpu_id: u64,
}

/// One validated logical-CPU/context association.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlicContext {
    pub(super) logical_cpu: LogicalCpuId,
    pub(super) hardware_cpu_id: u64,
    pub(super) index: usize,
}

/// Rejection while building or extending the controller context table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlicContextError {
    /// The raw context index is outside the register model.
    InvalidIndex { index: usize },
    /// The mapped window cannot cover the context's registers.
    InvalidAperture { index: usize, required: usize },
    /// A logical CPU, hardware CPU or raw index is already associated.
    Conflicting { index: usize },
    /// No usable S-mode context remains after runtime mapping.
    NoContext,
}

impl core::fmt::Display for PlicContextError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidIndex { index } => {
                write!(
                    formatter,
                    "PLIC context {index} is outside the register model"
                )
            }
            Self::InvalidAperture { index, required } => write!(
                formatter,
                "PLIC context {index} needs {required:#x} bytes of MMIO"
            ),
            Self::Conflicting { index } => {
                write!(
                    formatter,
                    "PLIC context {index} conflicts with an existing association"
                )
            }
            Self::NoContext => formatter.write_str("no usable PLIC S-mode context"),
        }
    }
}

/// Immutable logical-CPU-indexed context table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlicContexts {
    entries: [Option<PlicContext>; kbuild_config::NR_CPUS],
}

impl PlicContexts {
    pub(super) const fn new() -> Self {
        Self {
            entries: [None; kbuild_config::NR_CPUS],
        }
    }

    /// Maps firmware declarations to logical CPUs and validates their layout.
    ///
    /// A hardware identity outside the runtime CPU set is not a routing target;
    /// it is skipped without renumbering the raw context index. Identity
    /// conflicts, an unavailable register window or an empty result are errors.
    pub(super) fn build(
        declarations: &[PlicContextDecl],
        size: usize,
        mut map_cpu: impl FnMut(u64) -> Option<LogicalCpuId>,
    ) -> Result<Self, PlicContextError> {
        let mut contexts = Self::new();
        for declaration in declarations {
            let index = declaration.index;
            let required = PlicRegisters::required_aperture(index)
                .ok_or(PlicContextError::InvalidIndex { index })?;
            if size < required {
                return Err(PlicContextError::InvalidAperture { index, required });
            }
            let Some(logical_cpu) = map_cpu(declaration.hardware_cpu_id) else {
                continue;
            };
            contexts
                .insert(PlicContext {
                    logical_cpu,
                    hardware_cpu_id: declaration.hardware_cpu_id,
                    index,
                })
                .map_err(|()| PlicContextError::Conflicting { index })?;
        }
        if contexts.is_empty() {
            Err(PlicContextError::NoContext)
        } else {
            Ok(contexts)
        }
    }

    /// Records one association without overwriting or leaving partial state.
    pub(super) fn insert(&mut self, context: PlicContext) -> Result<(), ()> {
        let slot = context.logical_cpu.as_usize();
        if slot >= self.entries.len()
            || context.index >= super::registers::MAX_CONTEXT
            || self.entries[slot].is_some()
            || self.entries.iter().flatten().any(|existing| {
                existing.index == context.index
                    || existing.hardware_cpu_id == context.hardware_cpu_id
            })
        {
            return Err(());
        }
        self.entries[slot] = Some(context);
        Ok(())
    }

    pub(super) fn get(&self, logical_cpu: LogicalCpuId) -> Option<PlicContext> {
        self.entries.get(logical_cpu.as_usize()).copied().flatten()
    }

    #[cfg(unittest)]
    pub(super) fn iter(&self) -> impl Iterator<Item = PlicContext> + '_ {
        self.entries.iter().flatten().copied()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }
}
