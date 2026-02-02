#include <kcommon.h>
#include <kernel/pseudo_ta.h>
#include <kernel/tee_ta_manager.h>
#include <string.h>
#include <tee/uuid.h>
#include <user_ta_header.h>
#include <flashfs.h>
#include <kernel/panic.h>
#include <crypto/crypto.h>
#include <utee_defines.h>
#include <dynamic_measure.h>
#include <app_config.h>
#include <mm/core_memprot.h>
#include <mm/core_mmu.h>
#include <tee_internal_api.h>
#include "platform/tos_portable.h"
#include "pta_dice.h"

#include "dice/dice.h"
#include "tee_dice.h"
#include "dice/tee_ecdsa_utils.h"

#define STATIC_NAME "dice.pta"

static TEE_Result do_cmd_get_dice(uint32_t param_types,
				  TEE_Param params[4] __unused)
{
	uint32_t exp_param_types = TEE_PARAM_TYPES(
		TEE_PARAM_TYPE_MEMREF_INOUT, // handover (CDI_Attest, CDI_Seal, Cert_Chain)
		TEE_PARAM_TYPE_NONE, TEE_PARAM_TYPE_NONE, TEE_PARAM_TYPE_NONE);
	uint8_t cdi_buf[128];
	size_t actual_size;
	//TEE_Result res = TEE_SUCCESS;

	if (param_types != exp_param_types) {
		EMSG("Expected: 0x%x, got: 0x%x", exp_param_types, param_types);
		return TEE_ERROR_BAD_PARAMETERS;
	}

	if (kDiceResultOk !=
	    DiceTeeHandoverMainFlow(sizeof(cdi_buf), cdi_buf, &actual_size)) {
		return TEE_ERROR_GENERIC;
	}

	if (params[0].memref.size < actual_size) {
		EMSG("buffer size is invalid %x, need: %lx",
		     params[0].memref.size, actual_size);
		return TEE_ERROR_BAD_PARAMETERS;
	}

	memcpy(params[0].memref.buffer, cdi_buf, actual_size);

	EMSG("actual_size is: 0x%lx", actual_size);
	params[0].memref.size = actual_size;

	return TEE_SUCCESS;
}

#ifdef CFG_DICE_CHAIN

static TEE_Result do_cmd_get_dice_test(uint32_t param_types,
				       TEE_Param params[4] __unused)
{
#define CDI_BUF_SIZE 1500

	uint32_t exp_param_types =
		TEE_PARAM_TYPES(TEE_PARAM_TYPE_MEMREF_INOUT,
				TEE_PARAM_TYPE_NONE, TEE_PARAM_TYPE_NONE,
				TEE_PARAM_TYPE_NONE);

	if (param_types != exp_param_types) {
		EMSG("Expected: 0x%x, got: 0x%x", exp_param_types, param_types);
		return TEE_ERROR_BAD_PARAMETERS;
	}

	{
		uint8_t *cdi_chain_buf = malloc(CDI_BUF_SIZE);
		size_t cdi_chain_buf_sz = CDI_BUF_SIZE;

		DiceTeeHandoverMainFlowChain(CDI_BUF_SIZE, cdi_chain_buf,
					     &cdi_chain_buf_sz);
		DMSG("DiceTeeHandoverMainFlowChain %zu:", cdi_chain_buf_sz);
		DHEXDUMP(cdi_chain_buf, cdi_chain_buf_sz);

		free(cdi_chain_buf);
	}

	uint8_t public_key[P256_PUBLIC_KEY_SIZE] = { 0x0 };
	uint8_t private_key[P256_PRIVATE_KEY_SIZE] = { 0x0 };
	uint8_t seed[DICE_PRIVATE_KEY_SEED_SIZE] = { 0x0 };

	memset(seed, 0, sizeof(seed));

	int ret = P256KeypairFromSeed(public_key, private_key, seed);
	DMSG("P256KeypairFromSeed public_key:");
	DHEXDUMP(public_key, P256_PUBLIC_KEY_SIZE);
	DMSG("P256KeypairFromSeed private_key:");
	DHEXDUMP(private_key, P256_PRIVATE_KEY_SIZE);

	uint8_t signature[P256_SIGNATURE_SIZE] = { 0x0 };
	P256Sign(signature, (void *)"123", 3, private_key);
	DMSG("P256KeypairFromSeed signature:");
	DHEXDUMP(signature, P256_SIGNATURE_SIZE);

	ret = P256Verify((void *)"123", 3, signature, public_key);
	DMSG("P256Verify return: %x", ret);
	++signature[0];
	ret = P256Verify((void *)"123", 3, signature, public_key);
	DMSG("P256Verify after modify return: %x", ret);

	return TEE_SUCCESS;
}

static TEE_Result do_cmd_get_dice_chain(uint32_t param_types,
					TEE_Param params[4] __unused)
{
#define CDI_BUF_SIZE 1500
	TEE_Result res = TEE_SUCCESS;

	uint32_t exp_param_types = TEE_PARAM_TYPES(
		TEE_PARAM_TYPE_MEMREF_INOUT, // handover (CDI_Attest, CDI_Seal, Cert_Chain)
		TEE_PARAM_TYPE_NONE, TEE_PARAM_TYPE_NONE, TEE_PARAM_TYPE_NONE);
	uint8_t *cdi_chain_buf = NULL;

	if (param_types != exp_param_types) {
		EMSG("Expected: 0x%x, got: 0x%x", exp_param_types, param_types);
		return TEE_ERROR_BAD_PARAMETERS;
	}

	cdi_chain_buf = malloc(CDI_BUF_SIZE);
	size_t cdi_chain_buf_sz = CDI_BUF_SIZE;

	if (kDiceResultOk != DiceTeeHandoverMainFlowChain(CDI_BUF_SIZE,
							  cdi_chain_buf,
							  &cdi_chain_buf_sz)) {
		goto out;
	}

	DMSG("DiceTeeHandoverMainFlowChain %zu:", cdi_chain_buf_sz);
	DHEXDUMP(cdi_chain_buf, cdi_chain_buf_sz);

	if (params[0].memref.size < cdi_chain_buf_sz) {
		EMSG("buffer size is invalid %x, need: %lx",
		     params[0].memref.size, cdi_chain_buf_sz);
		res = TEE_ERROR_BAD_PARAMETERS;
		goto out;
	}

	memcpy(params[0].memref.buffer, cdi_chain_buf, cdi_chain_buf_sz);
	params[0].memref.size = cdi_chain_buf_sz;

out:
	free(cdi_chain_buf);

	return res;
}
#endif

static TEE_Result invoke_command(void *pSessionContext __unused,
				 uint32_t nCommandID, uint32_t nParamTypes,
				 TEE_Param pParams[TEE_NUM_PARAMS])
{
	TEE_Result res = TEE_SUCCESS;

	DMSG("invoke_command %d", nCommandID);

	switch (nCommandID) {
	case PTA_DICE_CMD_GET_CDI:
		res = do_cmd_get_dice(nParamTypes, pParams);
		break;
#ifdef CFG_DICE_CHAIN
	case PTA_DICE_CMD_GET_CDI_CHAIN:
		res = do_cmd_get_dice_chain(nParamTypes, pParams);
		break;
	case PTA_DICE_CMD_GET_CDI_TEST:
		res = do_cmd_get_dice_test(nParamTypes, pParams);
		break;
#endif
	default:
		res = TEE_ERROR_NOT_IMPLEMENTED;
		break;
	}

	return res;
}

static TEE_Result create_ta_dice(void)
{
	DMSG("------> create PTA \"%s\"", STATIC_NAME);
	return TEE_SUCCESS;
}

static void destroy_ta_dice(void)
{
	DMSG("<------ destroy PTA \"%s\"", STATIC_NAME);
}

pseudo_ta_register(.uuid = PTA_DICE_UUID, .name = STATIC_NAME,
		   .flags = PTA_DEFAULT_FLAGS,
		   .create_entry_point = create_ta_dice,
		   .destroy_entry_point = destroy_ta_dice,
		   .invoke_command_entry_point = invoke_command);