// #include <crypto/crypto.h>
// #include <kernel/panic.h>
// #include <kernel/pseudo_ta.h>
// #include <kernel/tee_ta_manager.h>
#include <string.h>
// #include <tee/uuid.h>

#include "dice/cbor_reader.h"
#include "dice/cbor_writer.h"
#include "dice/dice.h"
#include "dice/ops.h"
#include "dice/ops/trait/cose.h"
#include "dice/android.h"
// #include <kernel/huk_subkey.h>

// AndroidDiceHandover = {
//   1 : bstr .size 32,     ; CDI_Attest
//   2 : bstr .size 32,     ; CDI_Seal
//   ? 3 : DiceCertChain,   ; Android DICE chain
// }
static const int64_t kCdiAttestLabel = 1;
static const int64_t kCdiSealLabel = 2;
static const int64_t kDiceChainLabel = 3;
//
DiceResult DiceTeeHandoverMainFlow(size_t buffer_size, uint8_t *buffer,
								   size_t *actual_size)
{
	// Write the new handover data.
	struct CborOut out;
	CborOutInit(buffer, buffer_size, &out);
	CborWriteMap(/*num_pairs=*/2, &out);
	CborWriteInt(kCdiAttestLabel, &out);
	uint8_t *next_cdi_attest = CborAllocBstr(DICE_CDI_SIZE, &out);
	CborWriteInt(kCdiSealLabel, &out);
	uint8_t *next_cdi_seal = CborAllocBstr(DICE_CDI_SIZE, &out);
	// CborWriteInt(kDiceChainLabel, &out);

	DiceResult res;
	const char *data_cdi = "CDI_Attest";
	size_t data_cdi_len = strlen(data_cdi);
	res = DiceKdf(NULL, DICE_CDI_SIZE, data_cdi, data_cdi_len, data_cdi, data_cdi_len, data_cdi, data_cdi_len, (void *)(next_cdi_attest));
	if (kDiceResultOk != res)
	{
		return kDiceResultPlatformError;
	}

	data_cdi = "CDI_Seal";
	data_cdi_len = strlen(data_cdi);
	res = DiceKdf(NULL, DICE_CDI_SIZE, data_cdi, data_cdi_len, data_cdi, data_cdi_len, data_cdi, data_cdi_len, (void *)(next_cdi_seal));
	if (kDiceResultOk != res)
	{
		return kDiceResultPlatformError;
	}

	*actual_size = CborOutSize(&out);

	return kDiceResultOk;
}

static const uint8_t code_hash[DICE_HASH_SIZE] = {
	0x59, 0x0d, 0x30, 0x26, 0xdb, 0x37, 0xb7, 0x77, 0x98, 0x31, 0xf5,
	0xb7, 0x4f, 0xa4, 0x9a, 0xe4, 0x5d, 0x09, 0xc4, 0x6a, 0x50, 0x71,
	0x5a, 0xb0, 0x5e, 0x3d, 0xe2, 0xb6, 0x09, 0xf1, 0x82, 0x79, 0x03,
	0xeb, 0x9c, 0x29, 0x32, 0x13, 0xfe, 0x08, 0xc8, 0x7f, 0x35, 0x0f,
	0x86, 0x66, 0x2c, 0x99, 0x7d, 0x7b, 0x24, 0xff, 0xbb, 0xe0, 0x65,
	0x77, 0x2f, 0x84, 0x4a, 0xcb, 0x23, 0x7b, 0xf4, 0x90};

static const uint8_t config_value[DICE_INLINE_CONFIG_SIZE] = {
	0x83, 0x7b, 0x07, 0x82, 0xb8, 0x25, 0x61, 0xf3, 0x0a, 0xb3, 0x6f,
	0x95, 0x82, 0x93, 0xd5, 0x1d, 0x44, 0xaf, 0x04, 0x26, 0x94, 0x77,
	0x6d, 0x0f, 0x81, 0xee, 0xd7, 0x7d, 0xc3, 0xf6, 0x6a, 0x93, 0xd4,
	0x8f, 0x19, 0x7a, 0xad, 0x70, 0xbd, 0x41, 0xfc, 0x20, 0x20, 0x0e,
	0x29, 0x3e, 0xa9, 0x4d, 0x05, 0x56, 0x96, 0xf3, 0x8c, 0x51, 0x69,
	0x5b, 0xb0, 0xb6, 0xd3, 0xf2, 0xfe, 0x53, 0x96, 0xd0};

// #ifdef CFG_DICE_CHAIN
DiceResult DiceTeeHandoverMainFlowChainOrigin(uint8_t *buffer, size_t buffer_size,
											  size_t *actual_size)
{
	uint8_t handover[128] = {0};
	size_t handover_size = sizeof(handover);

	DiceInputValues input_values = {0};
	memcpy(input_values.code_hash, code_hash,
		   sizeof(input_values.code_hash));
	memcpy(input_values.config_value, config_value,
		   sizeof(input_values.config_value));
	memset(input_values.authority_hash, 0x00,
		   sizeof(input_values.authority_hash));

	DiceTeeHandoverMainFlow(sizeof(handover), handover, &handover_size);
	return DiceAndroidHandoverMainFlow(NULL, handover, handover_size,
									   &input_values, buffer_size, buffer,
									   actual_size);
}
// #endif

DiceResult DiceTeeHandoverMainFlowChain(const uint8_t *handover, size_t handover_size, uint8_t *buffer, size_t buffer_size,
										size_t *actual_size)
{
	DiceInputValues input_values = {0};
	memcpy(input_values.code_hash, code_hash,
		   sizeof(input_values.code_hash));
	memcpy(input_values.config_value, config_value,
		   sizeof(input_values.config_value));
	memset(input_values.authority_hash, 0x00,
		   sizeof(input_values.authority_hash));

	return DiceAndroidHandoverMainFlow(NULL, handover, handover_size,
									   &input_values, buffer_size, buffer,
									   actual_size);
}

DiceResult DiceTeeHandoverMainFlowChainCodeHash(const uint8_t *handover, size_t handover_size,
												const uint8_t *codehash, size_t codehash_size,
												uint8_t *buffer, size_t buffer_size,
												size_t *actual_size)
{
	DiceInputValues input_values = {0};
	size_t codehash_len = (codehash_size > sizeof(input_values.code_hash)) 
                        ? sizeof(input_values.code_hash) 
                        : codehash_size;
	memcpy(input_values.code_hash, codehash, codehash_len);
	memcpy(input_values.config_value, config_value,
		   sizeof(input_values.config_value));
	memset(input_values.authority_hash, 0x00,
		   sizeof(input_values.authority_hash));

	return DiceAndroidHandoverMainFlow(NULL, handover, handover_size,
									   &input_values, buffer_size, buffer,
									   actual_size);
}

void dice_init(void)
{
	DiceTeeHandoverMainFlowChainOrigin(256, NULL, NULL);
}