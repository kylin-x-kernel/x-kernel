// Copyright 2022 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may not
// use this file except in compliance with the License. You may obtain a copy of
// the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the
// License for the specific language governing permissions and limitations under
// the License.

// This is an implementation of the SM2 DSA crypto operations that uses boringssl.

#include "dice/mbedtls_sm2dsa_utils.h"
#if CFG_OPTEE_REVISION_MAJOR < 4
#include <mbedtls/version.h>
#else
#include <mbedtls/build_info.h>
#endif

#include <mbedtls/md.h>
#include <mbedtls/ecp.h>
#include <mbedtls/pk.h>
#include <mbedtls/platform.h>

int __attribute__((weak)) get_rand(unsigned char* output, size_t len);

int __attribute__((weak)) get_rand(unsigned char* output, size_t len) {
    FILE* f = fopen("/dev/urandom", "rb");
    if (f) {
        size_t read = fread(output, 1, len, f);
        fclose(f);
        if (read == len) {
            return 0;
        }
    }

    static int seeded = 0;
    if (!seeded) {
        srand((unsigned int)time(NULL));
        seeded = 1;
    }

    for (size_t i = 0; i < len; i++) {
        output[i] = (unsigned char)(rand() % 256);
    }

    return 0;
}

static int mbd_rand(void *rng_state, unsigned char *output,
			size_t len)
{
    (void)rng_state;
//	if (crypto_rng_read(output, len))
//		return MBEDTLS_ERR_CTR_DRBG_ENTROPY_SOURCE_FAILED;
    get_rand(output, len);
	return 0;
}

static int hmac(uint8_t key[64], uint8_t in[64], uint8_t *out,
				unsigned int out_len)
{
	int ret = -1;
	unsigned char output[MBEDTLS_MD_MAX_SIZE] = {0};
	const mbedtls_md_info_t *md_info = NULL;
	mbedtls_md_type_t md_alg = MBEDTLS_MD_SM3;
	mbedtls_md_context_t ctx;

	mbedtls_md_init(&ctx);
	md_info = mbedtls_md_info_from_type(md_alg);
	mbedtls_md_setup(&ctx, md_info, 1);

	ret = mbedtls_md_hmac_starts(&ctx, key, 64);
	if (ret != 0)
		goto exit;

	ret = mbedtls_md_hmac_update(&ctx, in, 64);
	if (ret != 0)
		goto exit;

	ret = mbedtls_md_hmac_finish(&ctx, out);
	if (ret != 0)
		goto exit;

exit:
	mbedtls_md_free(&ctx);
	return ret;
}

static int hmac3(uint8_t key[64], uint8_t in1[64], uint8_t in2,
				 const uint8_t *in3, unsigned int in3_len, uint8_t out[64])
{
	int ret = -1;
	unsigned char output[MBEDTLS_MD_MAX_SIZE] = {0};
	const mbedtls_md_info_t *md_info = NULL;
	mbedtls_md_type_t md_alg = MBEDTLS_MD_SM3;
	mbedtls_md_context_t ctx;

	mbedtls_md_init(&ctx);
	md_info = mbedtls_md_info_from_type(md_alg);
	mbedtls_md_setup(&ctx, md_info, 1);

	ret = mbedtls_md_hmac_starts(&ctx, key, 64);
	if (ret != 0)
		goto exit;

	ret = mbedtls_md_hmac_update(&ctx, in1, 64);
	if (ret != 0)
		goto exit;

	ret = mbedtls_md_hmac_update(&ctx, &in2, 1);
	if (ret != 0)
		goto exit;

	if (in3 != NULL && in3_len > 0)
	{
		ret = mbedtls_md_hmac_update(&ctx, in3, in3_len);
		if (ret != 0)
			goto exit;
	}

	ret = mbedtls_md_hmac_finish(&ctx, out);
	if (ret != 0)
		goto exit;

exit:
	mbedtls_md_free(&ctx);
	return ret;
}

static int derivePrivateKey(mbedtls_ecp_group *grp, const uint8_t *seed,
							size_t seed_size, size_t private_key_len,
							mbedtls_mpi *candidate)
{
	int ret = -1;
	uint8_t v[64];
	uint8_t k[64];
	memset(v, 1, 64);
	memset(k, 0, 64);

	if (private_key_len > 64)
	{
		goto err;
	}

	if (0 != hmac3(k, v, 0x00, seed, (unsigned int)seed_size, k))
	{
		goto err;
	}
	if (0 != hmac(k, v, v, sizeof(v)))
	{
		goto err;
	}
	if (0 != hmac3(k, v, 0x01, seed, (unsigned int)seed_size, k))
	{
		goto err;
	}
	mbedtls_mpi_init(candidate);
	do
	{
		if (0 != hmac(k, v, v, sizeof(v)))
		{
			goto err;
		}
		if (0 != hmac(k, v, v, sizeof(v)))
		{
			goto err;
		}
		if ((ret = mbedtls_mpi_read_binary(candidate, v,
										   private_key_len)) != 0)
			goto err;

		if (0 != hmac3(k, v, 0x00, NULL, 0, k))
		{
			goto err;
		}

	} while (mbedtls_mpi_cmp_mpi(candidate, &grp->N) >= 0 ||
			 mbedtls_mpi_cmp_int(candidate, 0) == 0);
	ret = 0;
	goto out;
err:
	mbedtls_mpi_free(candidate);
out:
	return ret;
}

static int KeypairFromSeed(int grp_id, uint8_t *public_key,
						   size_t public_key_size, uint8_t *private_key,
						   size_t private_key_size,
						   const uint8_t seed[DICE_PRIVATE_KEY_SEED_SIZE])
{
	int ret = -1;
	mbedtls_ecp_group grp;
	mbedtls_ecp_point Q;
	mbedtls_mpi d; /* private scalar */
	size_t coord_size;

	/* 初始化 */
	mbedtls_ecp_group_init(&grp);
	mbedtls_ecp_point_init(&Q);
	// mbedtls_mpi_init(&d);

	/* 载入曲线（grp_id 示例： MBEDTLS_ECP_DP_SECP256R1） */
	if ((ret = mbedtls_ecp_group_load(&grp, grp_id)) != 0)
	{
		ret = 0;
		goto cleanup;
	}

	/* 校验输出缓冲长度一致性 */
	if (public_key == NULL || private_key == NULL)
	{
		ret = 0;
		goto cleanup;
	}
	if (public_key_size % 2 != 0)
	{
		ret = 0;
		goto cleanup;
	}
	coord_size = public_key_size / 2;
	if (private_key_size == 0 || private_key_size > 64)
	{
		/* private_key_size 应该与曲线大小匹配（例如 P-256 为32） */
		ret = 0;
		goto cleanup;
	}

	/* 从 seed 派生私钥（假定 derivePrivateKey 返回 0 表示成功并填充 d） */
	if ((ret = derivePrivateKey(&grp, seed, DICE_PRIVATE_KEY_SEED_SIZE,
								private_key_size, &d)) != 0)
	{
		ret = 0;
		goto cleanup;
	}

	/* 导出私钥为定长大端字节串（会左侧填 0 以满足长度） */
	if ((ret = mbedtls_mpi_write_binary(&d, private_key,
										private_key_size)) != 0)
	{
		ret = 0;
		goto cleanup;
	}

	/* 计算公钥点 Q = d * G */
	if ((ret = mbedtls_ecp_mul(&grp, &Q, &d, &grp.G, mbd_rand, NULL)) != 0)
	{
		ret = 0;
		goto cleanup;
	}

	size_t ilen = 1 + 2 * coord_size;
	unsigned char *buf = malloc(ilen);
	if (buf == NULL)
	{
		ret = 0;
		goto cleanup;
	}
	size_t olen;
	if (0 != mbedtls_ecp_point_write_binary(&grp, &Q, MBEDTLS_ECP_PF_UNCOMPRESSED, &olen, buf, ilen))
	{
		ret = 0;
		free(buf);
		goto cleanup;
	}
	memcpy(public_key, buf + 1, 2 * coord_size);
	free(buf);
	ret = 1;

cleanup:
	/* 清理敏感数据 */
	mbedtls_mpi_free(&d);
	mbedtls_ecp_point_free(&Q);
	mbedtls_ecp_group_free(&grp);

	return ret;
}

int SM2KeypairFromSeed(uint8_t public_key[SM2_PUBLIC_KEY_SIZE],
					   uint8_t private_key[SM2_PRIVATE_KEY_SIZE],
					   const uint8_t seed[DICE_PRIVATE_KEY_SEED_SIZE])
{
	return KeypairFromSeed(MBEDTLS_ECP_DP_SM2P256R1, public_key,
						   SM2_PUBLIC_KEY_SIZE, private_key,
						   SM2_PRIVATE_KEY_SIZE, seed);
}

/*
 * SM2签名
 * message：待签名数据
 * message_size：数据长度
 * signature：签名
 * public_key：签名者私钥
 */
static int Sign(const uint8_t *message, size_t message_size,
				uint8_t signature[SM2_SIGNATURE_SIZE],
				const uint8_t private_key[SM2_PRIVATE_KEY_SIZE])
{
	// 定义一些过程中遇到的变量，包括hash类型、pk类型、椭圆曲线参数等等
	int ret = -1;
	mbedtls_md_type_t md_alg = MBEDTLS_MD_SM3;
	mbedtls_ecp_group_id grp_id = MBEDTLS_ECP_DP_SM2P256R1;

	mbedtls_sm2_context ctx;
	unsigned char hash_z[32];
	unsigned char hash_md[32];
	size_t slen = SM2_SIGNATURE_SIZE;

	// 随机数生成器
	uint8_t seed[DICE_PRIVATE_KEY_SEED_SIZE] = {0};
	size_t olen = 0;

	// 初始化sm2签名结构体
	mbedtls_sm2_init(&ctx);
	// 加载椭圆曲线
	if ((ret = mbedtls_ecp_group_load(&ctx.grp, grp_id)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	// 读取私钥
	if ((ret = mbedtls_mpi_read_binary(&ctx.d, private_key, 32)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	if ((ret = mbedtls_ecp_check_privkey(&ctx.grp, &ctx.d)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	// 计算公钥
	if ((ret = mbedtls_ecp_mul(&ctx.grp, &ctx.Q, &ctx.d, &ctx.grp.G, mbd_rand, NULL)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	// 计算e值和z值
	if ((ret = mbedtls_sm2_hash_z(&ctx, md_alg, NULL, 0, hash_z)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	if ((ret = mbedtls_sm2_hash_e(md_alg, hash_z, message, message_size, hash_md)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	// 签名
	if ((ret = mbedtls_sm2_sign_raw(&ctx, md_alg, hash_md, signature, &slen, mbd_rand, NULL)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	ret = 1;

	// 释放空间
cleanup:
	mbedtls_sm2_free(&ctx);
	return ret;
}

/*
 * SM2签名验证
 * message：待验证数据
 * message_size：数据长度
 * signature：待验证签名
 * public_key：签名者公钥
 */
static int Verify(const uint8_t *message, size_t message_size,
				  const uint8_t signature[SM2_SIGNATURE_SIZE],
				  const uint8_t public_key[SM2_PUBLIC_KEY_SIZE])
{
	// 定义一些过程中遇到的变量，包括hash类型、pk类型、椭圆曲线参数等等
	int ret = -1;
	mbedtls_md_type_t md_alg = MBEDTLS_MD_SM3;
	mbedtls_ecp_group_id grp_id = MBEDTLS_ECP_DP_SM2P256R1;

	mbedtls_sm2_context ctx;
	unsigned char hash_z[32];
	unsigned char hash_md[32];
	// 初始化结构体
	mbedtls_sm2_init(&ctx);
	// 加载曲线
	if ((ret = mbedtls_ecp_group_load(&ctx.grp, grp_id)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	// 读取公钥
	if ((ret = mbedtls_mpi_read_binary(&ctx.Q.X, public_key, 32)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	if ((ret = mbedtls_mpi_read_binary(&ctx.Q.Y, public_key + 32, 32)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	if ((ret = mbedtls_mpi_lset(&ctx.Q.Z, 1)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	// 计算z值和e值
	if ((ret = mbedtls_sm2_hash_z(&ctx, md_alg, NULL, 0, hash_z)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	if ((ret = mbedtls_sm2_hash_e(md_alg, hash_z, message, message_size, hash_md)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	// 签名验证
	if ((ret = mbedtls_sm2_verify_raw(&ctx, md_alg, hash_md, sizeof(hash_md), signature)) != 0)
	{
		ret = 0;
		goto cleanup;
	};
	ret = 1;

cleanup:
	// 释放空间
	mbedtls_sm2_free(&ctx);
	return ret;
}

int SM2Sign(uint8_t signature[SM2_SIGNATURE_SIZE], const uint8_t *message, size_t message_size,
			const uint8_t private_key[SM2_PRIVATE_KEY_SIZE])
{
	return Sign(message, message_size, signature, private_key);
}

int SM2Verify(const uint8_t *message, size_t message_size,
			  const uint8_t signature[SM2_SIGNATURE_SIZE],
			  const uint8_t public_key[SM2_PUBLIC_KEY_SIZE])
{
	return Verify(message, message_size, signature, public_key);
}