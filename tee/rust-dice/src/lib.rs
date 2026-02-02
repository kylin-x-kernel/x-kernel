#![no_std]
extern crate cty;

mod bindings {
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(non_upper_case_globals)]
    include!(concat!(env!("OUT_DIR"), "/dice-bindings.rs"));
}

pub use bindings::{
    DICE_ANDROID_CONFIG_COMPONENT_NAME, DICE_ANDROID_CONFIG_COMPONENT_VERSION,
    DICE_ANDROID_CONFIG_RESETTABLE, DICE_ANDROID_CONFIG_RKP_VM_MARKER,
    DICE_ANDROID_CONFIG_SECURITY_VERSION, DICE_HIDDEN_SIZE, DiceAndroidConfigValues,
    DiceAndroidFormatConfigDescriptor, DiceAndroidHandoverParse, DiceAndroidMainFlow,
    DiceConfigType, DiceConfigType_kDiceConfigTypeDescriptor, DiceConfigType_kDiceConfigTypeInline,
    DiceDeriveCdiPrivateKeySeed, DiceInputValues, DiceInputValues_, DiceMainFlow, DiceMode,
    DiceMode_kDiceModeDebug, DiceMode_kDiceModeNormal, DiceResult,
    DiceResult_kDiceResultBufferTooSmall, DiceResult_kDiceResultInvalidInput,
    DiceResult_kDiceResultOk, DiceResult_kDiceResultPlatformError, SM2KeypairFromSeed,
};

pub fn dice_parse_handover<'a>(buffer: &'a [u8]) -> Result<(&'a [u8], &'a [u8], &'a [u8]), u32> {
    let mut cdi_attest_ptr: *const u8 = core::ptr::null();
    let mut cdi_seal_ptr: *const u8 = core::ptr::null();
    let mut chain_ptr: *const u8 = core::ptr::null();
    let mut chain_size: usize = 0;

    let ret = unsafe {
        bindings::DiceAndroidHandoverParse(
            buffer.as_ptr(),
            buffer.len(),
            &mut cdi_attest_ptr,
            &mut cdi_seal_ptr,
            &mut chain_ptr,
            &mut chain_size,
        )
    };

    if ret != bindings::DiceResult_kDiceResultOk {
        return Err(ret);
    }

    // 注意：长度按 DICE CDI 定义通常是 32 字节
    let cdi_attest =
        unsafe { core::slice::from_raw_parts(cdi_attest_ptr, bindings::DICE_CDI_SIZE as usize) };
    let cdi_seal =
        unsafe { core::slice::from_raw_parts(cdi_seal_ptr, bindings::DICE_CDI_SIZE as usize) };
    let chain = unsafe { core::slice::from_raw_parts(chain_ptr, chain_size) };

    Ok((cdi_attest, cdi_seal, chain))
}

pub fn dice_main_flow_chain<'a>(handover: &[u8], buffer: &'a mut [u8]) -> Result<&'a [u8], u32> {
    let mut actual_size: usize = 0;

    let ret = unsafe {
        bindings::DiceTeeHandoverMainFlowChain(
            handover.as_ptr(),
            handover.len(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut actual_size,
        )
    };

    if ret != bindings::DiceResult_kDiceResultOk {
        return Err(ret);
    }

    // 返回 buffer[..actual_size] 作为只读 slice
    Ok(&buffer[..actual_size])
}

pub fn dice_main_flow_chain_codehash<'a>(
    handover: &[u8],
    codehash: &[u8],
    buffer: &'a mut [u8],
) -> Result<&'a [u8], u32> {
    let mut actual_size: usize = 0;

    let ret = unsafe {
        bindings::DiceTeeHandoverMainFlowChainCodeHash(
            handover.as_ptr(),
            handover.len(),
            codehash.as_ptr(),
            codehash.len(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut actual_size,
        )
    };

    if ret != bindings::DiceResult_kDiceResultOk {
        return Err(ret);
    }

    // 返回 buffer[..actual_size] 作为只读 slice
    Ok(&buffer[..actual_size])
}
