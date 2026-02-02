
#ifndef DCE_MBEDTLS_SM2DSA_UTILS_H_
#define DCE_MBEDTLS_SM2DSA_UTILS_H_

#include <stddef.h>
#include <stdint.h>

#include "dice/dice.h"

#ifdef __cplusplus
extern "C"
{
#endif

#define SM2_PRIVATE_KEY_SIZE 32
#define SM2_PUBLIC_KEY_SIZE 64
#define SM2_SIGNATURE_SIZE 64

int SM2KeypairFromSeed(uint8_t public_key[SM2_PUBLIC_KEY_SIZE],
						   uint8_t private_key[SM2_PRIVATE_KEY_SIZE],
						   const uint8_t seed[DICE_PRIVATE_KEY_SEED_SIZE]);

int SM2Sign(uint8_t signature[SM2_SIGNATURE_SIZE], const uint8_t *message, size_t message_size,
				const uint8_t private_key[SM2_PRIVATE_KEY_SIZE]);

int SM2Verify(const uint8_t *message, size_t message_size,
				  const uint8_t signature[SM2_SIGNATURE_SIZE],
				  const uint8_t public_key[SM2_PUBLIC_KEY_SIZE]);

	//void SM2UtilsTest(void);
#ifdef __cplusplus
} // extern "C"
#endif

#endif // DCE_MBEDTLS_SM2DSA_UTILS_H_
