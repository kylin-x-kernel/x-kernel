// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Validation helpers for interface declarations and providers.

use syn::{Error, FnArg, ImplItemFn, Signature, TraitItemFn, Visibility};

use crate::errors::generic_not_allowed_error;

/// Validates an interface method signature.
pub fn validate_signature(sig: &Signature) -> Result<(), Error> {
    if !sig.generics.params.is_empty() {
        return Err(generic_not_allowed_error(&sig.generics));
    }

    if sig.constness.is_some() {
        return Err(Error::new_spanned(
            sig,
            "const interface methods are not supported",
        ));
    }

    if sig.unsafety.is_some() {
        return Err(Error::new_spanned(
            sig,
            "unsafe interface methods are not supported",
        ));
    }

    if sig.asyncness.is_some() {
        return Err(Error::new_spanned(
            sig,
            "async interface methods are not supported",
        ));
    }

    if sig.abi.is_some() {
        return Err(Error::new_spanned(
            sig,
            "extern interface methods are not supported",
        ));
    }

    if sig.variadic.is_some() {
        return Err(Error::new_spanned(
            sig,
            "variadic interface methods are not supported",
        ));
    }

    for arg in &sig.inputs {
        if let FnArg::Receiver(receiver) = arg {
            return Err(Error::new_spanned(
                receiver,
                "stateless interface methods must not take a receiver",
            ));
        }
    }

    Ok(())
}

/// Validates an interface definition method.
pub fn validate_interface_method(method: &TraitItemFn) -> Result<(), Error> {
    if method.default.is_some() {
        return Err(Error::new_spanned(
            method,
            "interface methods must not have default bodies",
        ));
    }
    validate_signature(&method.sig)
}

/// Validates an interface provider method.
pub fn validate_interface_provider_method(method: &ImplItemFn) -> Result<(), Error> {
    if !matches!(method.vis, Visibility::Inherited) {
        return Err(Error::new_spanned(
            &method.vis,
            "provider methods must not declare visibility",
        ));
    }
    validate_signature(&method.sig)
}
