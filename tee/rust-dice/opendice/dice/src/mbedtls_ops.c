/*
 * @Author: 于博 yubo1@kylinos.cn
 * @Date: 2025-08-22 14:57:57
 * @LastEditors: 于博 yubo1@kylinos.cn
 * @LastEditTime: 2025-08-23 01:00:20
 * @FilePath: /optee_os/core/kylin_tos/pta/dice/mbedtls_ops.c
 * @Description: 这是默认设置,请设置`customMade`, 打开koroFileHeader查看配置 进行设置: https://github.com/OBKoro1/koro1FileHeader/wiki/%E9%85%8D%E7%BD%AE
 */
#include <stdint.h>
#include <string.h>

#include "dice/dice.h"
#include "dice/ops.h"

#include "mbedtls/hkdf.h"

DiceResult DiceHash(void* context_not_used, const uint8_t* input,
                    size_t input_size, uint8_t output[DICE_HASH_SIZE]) {
  (void)context_not_used;
  if (0 != mbedtls_md(mbedtls_md_info_from_type(MBEDTLS_MD_SHA512), input,
                      input_size, output)) {
    return kDiceResultPlatformError;
  }
  return kDiceResultOk;
}

DiceResult DiceKdf(void* context_not_used, size_t length, const uint8_t* ikm,
                   size_t ikm_size, const uint8_t* salt, size_t salt_size,
                   const uint8_t* info, size_t info_size, uint8_t* output) {
  (void)context_not_used;
  if (0 != mbedtls_hkdf(mbedtls_md_info_from_type(MBEDTLS_MD_SHA512), salt,
                        salt_size, ikm, ikm_size, info, info_size, output,
                        length)) {
    return kDiceResultPlatformError;
  }
  return kDiceResultOk;
}
