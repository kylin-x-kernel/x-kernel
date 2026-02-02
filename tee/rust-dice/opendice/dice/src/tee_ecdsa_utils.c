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

// This is an implementation of the ECDSA crypto operations that uses boringssl.

#include <stdint.h>
#include <string.h>
#include <stdlib.h>
//#include <assert.h>
struct bignum ;

#include "dice/tee_ecdsa_utils.h"
#include "mbedtls/md.h"
#include "mbedtls/ecp.h"
#include "dice/dice.h"
#include "mbedtls/version.h"
#include "mbedtls/ctr_drbg.h"
#include "mbedtls/ecdh.h"
#include "mbedtls/ecdsa.h"
#include "mbedtls/ecp.h"
#include "mbedtls/entropy.h"
#include "mbedtls/pk.h"
#include "mbedtls/bignum.h"

extern int get_rand(unsigned char* output, size_t len);

static inline int mbd_rand(void *rng_state, unsigned char *output,
			size_t len)
{
    (void)rng_state;
//	if (crypto_rng_read(output, len))
//		return MBEDTLS_ERR_CTR_DRBG_ENTROPY_SOURCE_FAILED;
    get_rand(output, len);
	return 0;
}

#define ciL    (sizeof(mbedtls_mpi_uint))         /* chars in limb  */
#define biL    (ciL << 3)               /* bits  in limb  */

#define BITS_TO_LIMBS(i)  ( (i) / biL + ( (i) % biL != 0 ) )

struct ecc_keypair {
	struct bignum *d;	/* Private value */
	struct bignum *x;	/* Public value x */
	struct bignum *y;	/* Public value y */
	uint32_t curve;	        /* Curve type */
	const struct crypto_ecc_keypair_ops *ops; /* Key Operations */
};

struct ecc_public_key {
	struct bignum *x;	/* Public value x */
	struct bignum *y;	/* Public value y */
	uint32_t curve;	        /* Curve type */
	const struct crypto_ecc_public_ops *ops; /* Key Operations */
};

/* List of Supported ECC Curves */
#define TEE_CRYPTO_ELEMENT_NONE             0x00000000
#define TEE_ECC_CURVE_NIST_P192             0x00000001
#define TEE_ECC_CURVE_NIST_P224             0x00000002
#define TEE_ECC_CURVE_NIST_P256             0x00000003
#define TEE_ECC_CURVE_NIST_P384             0x00000004
#define TEE_ECC_CURVE_NIST_P521             0x00000005
#define TEE_ECC_CURVE_25519                 0x00000300
#define TEE_ECC_CURVE_SM2                   0x00000400

/*
 * Fix GP Internal Core API v1.1
 *     "Table 6-12:  Structure of Algorithm Identifier"
 *     indicates ECDSA have the algorithm "0x41" and ECDH "0x42"
 * whereas
 *     "Table 6-11:  List of Algorithm Identifiers" defines
 *     TEE_ALG_ECDSA_P192 as 0x70001042
 *
 * We chose to define TEE_ALG_ECDSA_P192 as 0x70001041 (conform to table 6-12)
 */
#define TEE_ALG_ECDSA_P192                      0x70001041
#define TEE_ALG_ECDSA_P224                      0x70002041
#define TEE_ALG_ECDSA_P256                      0x70003041
#define TEE_ALG_ECDSA_P384                      0x70004041
#define TEE_ALG_ECDSA_P521                      0x70005041
#define TEE_ALG_ED25519                         0x70006043 /* v1.3.1 spec */
#define TEE_ALG_ECDH_P192                       0x80001042
#define TEE_ALG_ECDH_P224                       0x80002042
#define TEE_ALG_ECDH_P256                       0x80003042
#define TEE_ALG_ECDH_P384                       0x80004042
#define TEE_ALG_ECDH_P521                       0x80005042
#define TEE_ALG_SM2_PKE                         0x80000045
#define TEE_ALG_SM3                             0x50000007
#define TEE_ALG_X25519                          0x80000044
#define TEE_ALG_ILLEGAL_VALUE                   0xEFFFFFFF
#define TEE_ALG_SM2_DSA_SM3                     0x70006045
#define TEE_ALG_DH_DERIVE_SHARED_SECRET         0x80000032
#define TEE_ALG_SM2_KEP                         0x60000045

#define TEE_SHA256_HASH_SIZE 32
#define TEE_TYPE_ECDSA_KEYPAIR              0xA1000041
#define TEE_ALG_SHA256                          0x50000004
#define TEE_TYPE_ECDSA_PUBLIC_KEY           0xA0000041
#define CFG_CORE_BIGNUM_MAX_BITS 4096

typedef uint32_t TEE_Result;

/* API Error Codes */
#define TEE_SUCCESS                       0x00000000
#define TEE_ERROR_CORRUPT_OBJECT          0xF0100001
#define TEE_ERROR_CORRUPT_OBJECT_2        0xF0100002
#define TEE_ERROR_STORAGE_NOT_AVAILABLE   0xF0100003
#define TEE_ERROR_STORAGE_NOT_AVAILABLE_2 0xF0100004
#define TEE_ERROR_CIPHERTEXT_INVALID      0xF0100006
#define TEE_ERROR_GENERIC                 0xFFFF0000
#define TEE_ERROR_ACCESS_DENIED           0xFFFF0001
#define TEE_ERROR_CANCEL                  0xFFFF0002
#define TEE_ERROR_ACCESS_CONFLICT         0xFFFF0003
#define TEE_ERROR_EXCESS_DATA             0xFFFF0004
#define TEE_ERROR_BAD_FORMAT              0xFFFF0005
#define TEE_ERROR_BAD_PARAMETERS          0xFFFF0006
#define TEE_ERROR_BAD_STATE               0xFFFF0007
#define TEE_ERROR_ITEM_NOT_FOUND          0xFFFF0008
#define TEE_ERROR_NOT_IMPLEMENTED         0xFFFF0009
#define TEE_ERROR_NOT_SUPPORTED           0xFFFF000A
#define TEE_ERROR_NO_DATA                 0xFFFF000B
#define TEE_ERROR_OUT_OF_MEMORY           0xFFFF000C
#define TEE_ERROR_BUSY                    0xFFFF000D
#define TEE_ERROR_COMMUNICATION           0xFFFF000E
#define TEE_ERROR_SECURITY                0xFFFF000F
#define TEE_ERROR_SHORT_BUFFER            0xFFFF0010
#define TEE_ERROR_EXTERNAL_CANCEL         0xFFFF0011
#define TEE_ERROR_OVERFLOW                0xFFFF300F
#define TEE_ERROR_TARGET_DEAD             0xFFFF3024
#define TEE_ERROR_STORAGE_NO_SPACE        0xFFFF3041
#define TEE_ERROR_MAC_INVALID             0xFFFF3071
#define TEE_ERROR_SIGNATURE_INVALID       0xFFFF3072
#define TEE_ERROR_TIME_NOT_SET            0xFFFF5000
#define TEE_ERROR_TIME_NEEDS_RESET        0xFFFF5001


/*
 * curve is part of TEE_ECC_CURVE_NIST_P192,...
 * algo is part of TEE_ALG_ECDSA_P192,..., and 0 if we do not have it
 */
static TEE_Result ecc_get_keysize(uint32_t curve, uint32_t algo,
				  size_t *key_size_bytes, size_t *key_size_bits)
{
	/*
	 * Note GPv1.1 indicates TEE_ALG_ECDH_NIST_P192_DERIVE_SHARED_SECRET
	 * but defines TEE_ALG_ECDH_P192
	 */
	switch (curve) {
	case TEE_ECC_CURVE_NIST_P192:
		*key_size_bits = 192;
		*key_size_bytes = 24;
		if ((algo != 0) && (algo != TEE_ALG_ECDSA_P192) &&
		    (algo != TEE_ALG_ECDH_P192))
			return TEE_ERROR_BAD_PARAMETERS;
		break;
	case TEE_ECC_CURVE_NIST_P224:
		*key_size_bits = 224;
		*key_size_bytes = 28;
		if ((algo != 0) && (algo != TEE_ALG_ECDSA_P224) &&
		    (algo != TEE_ALG_ECDH_P224))
			return TEE_ERROR_BAD_PARAMETERS;
		break;
	case TEE_ECC_CURVE_NIST_P256:
		*key_size_bits = 256;
		*key_size_bytes = 32;
		if ((algo != 0) && (algo != TEE_ALG_ECDSA_P256) &&
		    (algo != TEE_ALG_ECDH_P256))
			return TEE_ERROR_BAD_PARAMETERS;
		break;
	case TEE_ECC_CURVE_NIST_P384:
		*key_size_bits = 384;
		*key_size_bytes = 48;
		if ((algo != 0) && (algo != TEE_ALG_ECDSA_P384) &&
		    (algo != TEE_ALG_ECDH_P384))
			return TEE_ERROR_BAD_PARAMETERS;
		break;
	case TEE_ECC_CURVE_NIST_P521:
		*key_size_bits = 521;
		*key_size_bytes = 66;
		if ((algo != 0) && (algo != TEE_ALG_ECDSA_P521) &&
		    (algo != TEE_ALG_ECDH_P521))
			return TEE_ERROR_BAD_PARAMETERS;
		break;
	case TEE_ECC_CURVE_SM2:
		*key_size_bits = 256;
		*key_size_bytes = 32;
		if (algo != 0 && algo != TEE_ALG_SM2_DSA_SM3 &&
		    algo != TEE_ALG_SM2_KEP && algo != TEE_ALG_SM2_PKE)
			return TEE_ERROR_BAD_PARAMETERS;
		break;
	default:
		*key_size_bits = 0;
		*key_size_bytes = 0;
		return TEE_ERROR_NOT_SUPPORTED;
	}

	return TEE_SUCCESS;
}

static mbedtls_ecp_group_id curve_to_group_id(uint32_t curve)
{
//	DMSG("Transfer ECC curve to Group ID:");
	switch (curve) {
	case TEE_ECC_CURVE_NIST_P192:
//		DMSG("TEE_ECC_CURVE_NIST_P192 to MBEDTLS_ECP_DP_SECP192R1");
		return MBEDTLS_ECP_DP_SECP192R1;
	case TEE_ECC_CURVE_NIST_P224:
//		DMSG("TEE_ECC_CURVE_NIST_P224 to MBEDTLS_ECP_DP_SECP224R1");
		return MBEDTLS_ECP_DP_SECP224R1;
	case TEE_ECC_CURVE_NIST_P256:
//		DMSG("TEE_ECC_CURVE_NIST_P256 to MBEDTLS_ECP_DP_SECP256R1");
		return MBEDTLS_ECP_DP_SECP256R1;
	case TEE_ECC_CURVE_NIST_P384:
//		DMSG("TEE_ECC_CURVE_NIST_P384 to MBEDTLS_ECP_DP_SECP384R1");
		return MBEDTLS_ECP_DP_SECP384R1;
	case TEE_ECC_CURVE_NIST_P521:
//		DMSG("TEE_ECC_CURVE_NIST_P521 to MBEDTLS_ECP_DP_SECP521R1");
		return MBEDTLS_ECP_DP_SECP521R1;
//	case TEE_ECC_CURVE_SM2:
//		DMSG("TEE_ECC_CURVE_SM2 to MBEDTLS_ECP_DP_SM2");
//		return MBEDTLS_ECP_DP_SM2;
	default:
//		EMSG("Invalid ECC curve");
		return MBEDTLS_ECP_DP_NONE;
	}
}


static int hmac(uint8_t k[64], uint8_t in[64], uint8_t *out,
                unsigned int out_len)
{
    int ret = 0;
    mbedtls_md_context_t ctx;
    const mbedtls_md_info_t *info = mbedtls_md_info_from_type(MBEDTLS_MD_SHA512);

    if (info == NULL)
        return -1; // 不支持 SHA512

    mbedtls_md_init(&ctx);

    ret = mbedtls_md_setup(&ctx, info, 1 /* HMAC 模式 */);
    if (ret != 0)
        goto out;

    ret = mbedtls_md_hmac_starts(&ctx, k, 64);
    if (ret != 0)
        goto out;

    ret = mbedtls_md_hmac_update(&ctx, in, 64);
    if (ret != 0)
        goto out;

    ret = mbedtls_md_hmac_finish(&ctx, out);
    if (ret != 0)
        goto out;
out:
    mbedtls_md_free(&ctx);
    return ret;
}


static int hmac3(uint8_t k[64], uint8_t in1[64], uint8_t in2,
                 const uint8_t *in3, unsigned int in3_len, uint8_t out[64])
{
    int ret = 0;
    mbedtls_md_context_t ctx;
    const mbedtls_md_info_t *info = mbedtls_md_info_from_type(MBEDTLS_MD_SHA512);

    if (info == NULL)
        return MBEDTLS_ERR_MD_FEATURE_UNAVAILABLE;

    mbedtls_md_init(&ctx);

    // 第三个参数 = 1 表示启用 HMAC
    ret = mbedtls_md_setup(&ctx, info, 1);
    if (ret != 0)
        goto cleanup;

    ret = mbedtls_md_hmac_starts(&ctx, k, 64);
    if (ret != 0)
        goto cleanup;

    ret = mbedtls_md_hmac_update(&ctx, in1, 64);
    if (ret != 0)
        goto cleanup;

    ret = mbedtls_md_hmac_update(&ctx, &in2, 1);
    if (ret != 0)
        goto cleanup;

    if (in3 != NULL && in3_len > 0) {
        ret = mbedtls_md_hmac_update(&ctx, in3, in3_len);
        if (ret != 0)
            goto cleanup;
    }

    ret = mbedtls_md_hmac_finish(&ctx, out);
    // out 的长度由算法决定，这里是 64 字节（SHA-512）

cleanup:
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

	if (private_key_len > 64) {
		goto err;
	}

	if (0 != hmac3(k, v, 0x00, seed, (unsigned int)seed_size, k)) {
		goto err;
	}
	if (0 != hmac(k, v, v, sizeof(v))) {
		goto err;
	}
	if (0 != hmac3(k, v, 0x01, seed, (unsigned int)seed_size, k)) {
		goto err;
	}
	mbedtls_mpi_init(candidate);
	do {
		if (0 != hmac(k, v, v, sizeof(v))) {
			goto err;
		}
		if (0 != hmac(k, v, v, sizeof(v))) {
			goto err;
		}
		if ((ret = mbedtls_mpi_read_binary(candidate, v,
						   private_key_len)) != 0)
			goto err;

		if (0 != hmac3(k, v, 0x00, NULL, 0, k)) {
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
	int ret_code =
		0; /* 0 = fail (default), 1 = success to match original OpenSSL function) */
	int ret;
	mbedtls_ecp_group grp;
	mbedtls_ecp_point Q;
	mbedtls_mpi d; /* private scalar */
	size_t coord_size;

	/* 初始化 */
	mbedtls_ecp_group_init(&grp);
	mbedtls_ecp_point_init(&Q);
	//mbedtls_mpi_init(&d);

	/* 载入曲线（grp_id 示例： MBEDTLS_ECP_DP_SECP256R1） */
	if ((ret = mbedtls_ecp_group_load(&grp, grp_id)) != 0) {
		goto cleanup;
	}

	/* 校验输出缓冲长度一致性 */
	if (public_key == NULL || private_key == NULL) {
		goto cleanup;
	}
	if (public_key_size % 2 != 0) {
		goto cleanup;
	}
	coord_size = public_key_size / 2;
	if (private_key_size == 0 || private_key_size > 64) {
		/* private_key_size 应该与曲线大小匹配（例如 P-256 为32） */
		goto cleanup;
	}

	/* 从 seed 派生私钥（假定 derivePrivateKey 返回 0 表示成功并填充 d） */
	if ((ret = derivePrivateKey(&grp, seed, DICE_PRIVATE_KEY_SEED_SIZE,
				    private_key_size, &d)) != 0) {
		goto cleanup;
	}

	/* 导出私钥为定长大端字节串（会左侧填 0 以满足长度） */
	if ((ret = mbedtls_mpi_write_binary(&d, private_key,
					    private_key_size)) != 0) {
		goto cleanup;
	}

	/* 计算公钥点 Q = d * G */
	if ((ret = mbedtls_ecp_mul(&grp, &Q, &d, &grp.G, mbd_rand, NULL)) != 0) {
		goto cleanup;
	}

	/* 将公钥坐标 X, Y 写为定长大端字节串（每个 coord_size 字节） */
	/* mbedtls_mpi_write_binary 会按 big-endian 写出恰好指定长度（高位填0） */
	// if ((ret = mbedtls_mpi_write_binary(&Q.X, public_key + 0,
	// 				    coord_size)) != 0) {
	// 	goto cleanup;
	// }
	// if ((ret = mbedtls_mpi_write_binary(&Q.Y, public_key + coord_size,
	// 				    coord_size)) != 0) {
	// 	goto cleanup;
	// }
	size_t ilen = 1 + 2 * coord_size;
	unsigned char *buf = malloc(ilen);
	if (buf == NULL) {
		goto cleanup;
	}
	size_t olen;
	if (0 != mbedtls_ecp_point_write_binary(&grp, &Q, MBEDTLS_ECP_PF_UNCOMPRESSED, &olen, buf, ilen)) {
		free(buf);
		goto cleanup;
	}
	memcpy(public_key, buf+1, 2*coord_size);
	free(buf);

	/* 成功 */
	ret_code = 1;

cleanup:
	/* 清理敏感数据 */
	mbedtls_mpi_free(&d);
	mbedtls_ecp_point_free(&Q);
	mbedtls_ecp_group_free(&grp);

	return ret_code;
}

int P256KeypairFromSeed(uint8_t public_key[P256_PUBLIC_KEY_SIZE],
			uint8_t private_key[P256_PRIVATE_KEY_SIZE],
			const uint8_t seed[DICE_PRIVATE_KEY_SEED_SIZE])
{
	return KeypairFromSeed(MBEDTLS_ECP_DP_SECP256R1, public_key,
			       P256_PUBLIC_KEY_SIZE, private_key,
			       P256_PRIVATE_KEY_SIZE, seed);
}

static int digest_msg(mbedtls_md_type_t md_algo,
                      const uint8_t *msg, size_t msg_sz,
                      uint8_t *hash)
{
    int ret = 0;
    mbedtls_md_context_t ctx;
    const mbedtls_md_info_t *info = NULL;

    info = mbedtls_md_info_from_type(md_algo);
    if (info == NULL)
        return MBEDTLS_ERR_MD_FEATURE_UNAVAILABLE;

    mbedtls_md_init(&ctx);

    ret = mbedtls_md_setup(&ctx, info, 0);  // 0 表示非 HMAC
    if (ret != 0)
        goto cleanup;

    ret = mbedtls_md_starts(&ctx);
    if (ret != 0)
        goto cleanup;

    ret = mbedtls_md_update(&ctx, msg, msg_sz);
    if (ret != 0)
        goto cleanup;

    ret = mbedtls_md_finish(&ctx, hash);
    if (ret != 0)
        goto cleanup;

cleanup:
    mbedtls_md_free(&ctx);
    return ret;  // 0 表示成功，负值表示失败
}

#if MBEDTLS_VERSION_NUMBER < 0x03000000

void crypto_bignum_free(struct bignum *s)
{
	mbedtls_mpi_free((mbedtls_mpi *)s);
	free(s);
}

static void ecc_keypair_free(struct ecc_keypair *ecc_keypair)
{
	if (!ecc_keypair)
		return;

	/* free the sign key */
	crypto_bignum_free(ecc_keypair->d);
	crypto_bignum_free(ecc_keypair->x);
	crypto_bignum_free(ecc_keypair->y);
}
#endif

//
//TEE_Result crypto_rng_read(void *buf, size_t blen)
//{
//    if (!buf || blen == 0)
//        return TEE_ERROR_GENERIC;
//
//    int ret;
//    mbedtls_entropy_context entropy;
//    mbedtls_ctr_drbg_context ctr_drbg;
//
//    // 初始化
//    mbedtls_entropy_init(&entropy);
//    mbedtls_ctr_drbg_init(&ctr_drbg);
//
//    const char *pers = "tee_crypto_rng";
//
//    // CTR-DRBG 种子初始化
//    ret = mbedtls_ctr_drbg_seed(&ctr_drbg, mbedtls_entropy_func,
//                                &entropy,
//                                (const unsigned char *)pers,
//                                strlen(pers));
//    if (ret != 0) {
//        mbedtls_ctr_drbg_free(&ctr_drbg);
//        mbedtls_entropy_free(&entropy);
//        return TEE_ERROR_GENERIC;
//    }
//
//    // 生成随机数
//    ret = mbedtls_ctr_drbg_random(&ctr_drbg, (unsigned char *)buf, blen);
//    if (ret != 0) {
//        mbedtls_ctr_drbg_free(&ctr_drbg);
//        mbedtls_entropy_free(&entropy);
//        return TEE_ERROR_GENERIC;
//    }
//
//    // 清理
//    mbedtls_ctr_drbg_free(&ctr_drbg);
//    mbedtls_entropy_free(&entropy);
//
//    return TEE_SUCCESS;
//}

static TEE_Result ecc_sign(uint32_t algo, struct ecc_keypair *key,
			   const uint8_t *msg, size_t msg_len, uint8_t *sig,
			   size_t *sig_len)
{
	TEE_Result res = TEE_SUCCESS;
	int lmd_res = 0;
	const mbedtls_pk_info_t *pk_info = NULL;
	mbedtls_ecdsa_context ecdsa;
	mbedtls_ecp_group_id gid;
	size_t key_size_bytes = 0;
	size_t key_size_bits = 0;
	mbedtls_mpi r;
	mbedtls_mpi s;

	memset(&ecdsa, 0, sizeof(ecdsa));
	memset(&gid, 0, sizeof(gid));
	memset(&r, 0, sizeof(r));
	memset(&s, 0, sizeof(s));

	if (algo == 0)
		return TEE_ERROR_BAD_PARAMETERS;

	mbedtls_mpi_init(&r);
	mbedtls_mpi_init(&s);

	mbedtls_ecdsa_init(&ecdsa);

	gid = curve_to_group_id(key->curve);
	lmd_res = mbedtls_ecp_group_load(&ecdsa.grp, gid);
	if (lmd_res != 0) {
		res = TEE_ERROR_NOT_SUPPORTED;
		goto out;
	}

	ecdsa.d = *(mbedtls_mpi *)key->d;

	res = ecc_get_keysize(key->curve, algo, &key_size_bytes,
			      &key_size_bits);
	if (res != TEE_SUCCESS)
		goto out;

	if (*sig_len < 2 * key_size_bytes) {
		*sig_len = 2 * key_size_bytes;
		res = TEE_ERROR_SHORT_BUFFER;
		goto out;
	}

	pk_info = mbedtls_pk_info_from_type(MBEDTLS_PK_ECDSA);
	if (pk_info == NULL) {
		res = TEE_ERROR_NOT_SUPPORTED;
		goto out;
	}

	lmd_res = mbedtls_ecdsa_sign(&ecdsa.grp, &r, &s, &ecdsa.d, msg,
				     msg_len, mbd_rand, NULL);
	if (lmd_res == 0) {
		*sig_len = 2 * key_size_bytes;
		memset(sig, 0, *sig_len);
		mbedtls_mpi_write_binary(&r, sig + *sig_len / 2 -
					 mbedtls_mpi_size(&r),
					 mbedtls_mpi_size(&r));

		mbedtls_mpi_write_binary(&s, sig + *sig_len -
					 mbedtls_mpi_size(&s),
					 mbedtls_mpi_size(&s));
		res = TEE_SUCCESS;
	} else {
//		FMSG("mbedtls_ecdsa_sign failed, returned 0x%x\n", -lmd_res);
		res = TEE_ERROR_GENERIC;
	}
out:
	mbedtls_mpi_free(&r);
	mbedtls_mpi_free(&s);
	/* Reset mpi to skip freeing here, those mpis will be freed with key */
	mbedtls_mpi_init(&ecdsa.d);
	mbedtls_ecdsa_free(&ecdsa);
	return res;
}


/* Translate mbedtls result to TEE result */
static TEE_Result get_tee_result(int lmd_res)
{
	switch (lmd_res) {
	case 0:
		return TEE_SUCCESS;
	case MBEDTLS_ERR_ECP_VERIFY_FAILED:
		return TEE_ERROR_SIGNATURE_INVALID;
	case MBEDTLS_ERR_ECP_BUFFER_TOO_SMALL:
		return TEE_ERROR_SHORT_BUFFER;
	default:
		return TEE_ERROR_BAD_STATE;
	}
}

static TEE_Result ecc_verify(uint32_t algo, struct ecc_public_key *key,
			     const uint8_t *msg, size_t msg_len,
			     const uint8_t *sig, size_t sig_len)
{
	TEE_Result res = TEE_SUCCESS;
	int lmd_res = 0;
	mbedtls_ecdsa_context ecdsa;
	mbedtls_ecp_group_id gid;
	size_t key_size_bytes, key_size_bits = 0;
	uint8_t one[1] = { 1 };
	mbedtls_mpi r;
	mbedtls_mpi s;

	memset(&ecdsa, 0, sizeof(ecdsa));
	memset(&gid, 0, sizeof(gid));
	memset(&r, 0, sizeof(r));
	memset(&s, 0, sizeof(s));

	if (algo == 0)
		return TEE_ERROR_BAD_PARAMETERS;

	mbedtls_mpi_init(&r);
	mbedtls_mpi_init(&s);

	mbedtls_ecdsa_init(&ecdsa);

	gid = curve_to_group_id(key->curve);
	lmd_res = mbedtls_ecp_group_load(&ecdsa.grp, gid);
	if (lmd_res != 0) {
		res = TEE_ERROR_NOT_SUPPORTED;
		goto out;
	}

	ecdsa.Q.X = *(mbedtls_mpi *)key->x;
	ecdsa.Q.Y = *(mbedtls_mpi *)key->y;
	mbedtls_mpi_read_binary(&ecdsa.Q.Z, one, sizeof(one));

	res = ecc_get_keysize(key->curve, algo,
			      &key_size_bytes, &key_size_bits);
	if (res != TEE_SUCCESS) {
		res = TEE_ERROR_BAD_PARAMETERS;
		goto out;
	}

	/* check keysize vs sig_len */
	if ((key_size_bytes * 2) != sig_len) {
		res = TEE_ERROR_BAD_PARAMETERS;
		goto out;
	}

	mbedtls_mpi_read_binary(&r, sig, sig_len / 2);
	mbedtls_mpi_read_binary(&s, sig + sig_len / 2, sig_len / 2);

	lmd_res = mbedtls_ecdsa_verify(&ecdsa.grp, msg, msg_len, &ecdsa.Q,
				       &r, &s);
	if (lmd_res != 0) {
//		FMSG("mbedtls_ecdsa_verify failed, returned 0x%x", -lmd_res);
		res = get_tee_result(lmd_res);
	}
out:
	mbedtls_mpi_free(&r);
	mbedtls_mpi_free(&s);
	/* Reset mpi to skip freeing here, those mpis will be freed with key */
	mbedtls_mpi_init(&ecdsa.Q.X);
	mbedtls_mpi_init(&ecdsa.Q.Y);
	mbedtls_ecdsa_free(&ecdsa);
	return res;
}


TEE_Result crypto_acipher_ecc_sign(uint32_t algo, struct ecc_keypair *key,
				   const uint8_t *msg, size_t msg_len,
				   uint8_t *sig, size_t *sig_len)
{
//	assert(key->ops);
//
//	if (!key->ops->sign)
//		return TEE_ERROR_NOT_IMPLEMENTED;

	return ecc_sign(algo, key, msg, msg_len, sig, sig_len);
}

TEE_Result crypto_acipher_ecc_verify(uint32_t algo, struct ecc_public_key *key,
				     const uint8_t *msg, size_t msg_len,
				     const uint8_t *sig, size_t sig_len)
{
//	assert(key->ops);
//
//	if (!key->ops->verify)
//		return TEE_ERROR_NOT_IMPLEMENTED;

	return ecc_verify(algo, key, msg, msg_len, sig, sig_len);
}

struct bignum *crypto_bignum_allocate(size_t size_bits)
{
	mbedtls_mpi *bn = NULL;

	if (size_bits > CFG_CORE_BIGNUM_MAX_BITS)
		size_bits = CFG_CORE_BIGNUM_MAX_BITS;

	bn = calloc(1, sizeof(mbedtls_mpi));
	if (!bn)
		return NULL;
	mbedtls_mpi_init(bn);
	if (mbedtls_mpi_grow(bn, BITS_TO_LIMBS(size_bits)) != 0) {
		free(bn);
		return NULL;
	}

	return (struct bignum *)bn;
}

TEE_Result crypto_asym_alloc_ecc_keypair(struct ecc_keypair *s,
					 uint32_t key_type,
					 size_t key_size_bits)
{
	memset(s, 0, sizeof(*s));
//
//	switch (key_type) {
//	case TEE_TYPE_ECDSA_KEYPAIR:
//	case TEE_TYPE_ECDH_KEYPAIR:
//		s->ops = &ecc_keypair_ops;
//		break;
//	case TEE_TYPE_SM2_DSA_KEYPAIR:
//		if (!IS_ENABLED(CFG_CRYPTO_SM2_DSA))
//			return TEE_ERROR_NOT_IMPLEMENTED;
//
//		s->curve = TEE_ECC_CURVE_SM2;
//		s->ops = &sm2_dsa_keypair_ops;
//		break;
//	case TEE_TYPE_SM2_PKE_KEYPAIR:
//		if (!IS_ENABLED(CFG_CRYPTO_SM2_PKE))
//			return TEE_ERROR_NOT_IMPLEMENTED;
//
//		s->curve = TEE_ECC_CURVE_SM2;
//		s->ops = &sm2_pke_keypair_ops;
//		break;
//	case TEE_TYPE_SM2_KEP_KEYPAIR:
//		if (!IS_ENABLED(CFG_CRYPTO_SM2_KEP))
//			return TEE_ERROR_NOT_IMPLEMENTED;
//
//		s->curve = TEE_ECC_CURVE_SM2;
//		s->ops = &sm2_kep_keypair_ops;
//		break;
//	default:
//		return TEE_ERROR_NOT_IMPLEMENTED;
//	}

	s->d = crypto_bignum_allocate(key_size_bits);
	if (!s->d)
		goto err;
	s->x = crypto_bignum_allocate(key_size_bits);
	if (!s->x)
		goto err;
	s->y = crypto_bignum_allocate(key_size_bits);
	if (!s->y)
		goto err;

	return TEE_SUCCESS;

err:
	crypto_bignum_free(s->d);
	crypto_bignum_free(s->x);

	return TEE_ERROR_OUT_OF_MEMORY;
}

TEE_Result crypto_acipher_alloc_ecc_keypair(struct ecc_keypair *key,
					    uint32_t key_type,
					    size_t key_size_bits)
{
	TEE_Result res = TEE_ERROR_NOT_IMPLEMENTED;

	res = crypto_asym_alloc_ecc_keypair(key, key_type,
						   key_size_bits);
	return res;
}

TEE_Result crypto_bignum_bin2bn(const uint8_t *from, size_t fromsize,
			 struct bignum *to)
{
//	assert(from != NULL);
//	assert(to != NULL);
	if (mbedtls_mpi_read_binary((mbedtls_mpi *)to, from, fromsize))
		return TEE_ERROR_BAD_PARAMETERS;
	return TEE_SUCCESS;
}

TEE_Result crypto_asym_alloc_ecc_public_key(struct ecc_public_key *s,
					    uint32_t key_type,
					    size_t key_size_bits)
{
	memset(s, 0, sizeof(*s));
//
//	switch (key_type) {
//	case TEE_TYPE_ECDSA_PUBLIC_KEY:
//	case TEE_TYPE_ECDH_PUBLIC_KEY:
//		s->ops = &ecc_public_key_ops;
//		break;
//	case TEE_TYPE_SM2_DSA_PUBLIC_KEY:
//		if (!IS_ENABLED(CFG_CRYPTO_SM2_DSA))
//			return TEE_ERROR_NOT_IMPLEMENTED;
//
//		s->curve = TEE_ECC_CURVE_SM2;
//		s->ops = &sm2_dsa_public_key_ops;
//		break;
//	case TEE_TYPE_SM2_PKE_PUBLIC_KEY:
//		if (!IS_ENABLED(CFG_CRYPTO_SM2_PKE))
//			return TEE_ERROR_NOT_IMPLEMENTED;
//
//		s->curve = TEE_ECC_CURVE_SM2;
//		s->ops = &sm2_pke_public_key_ops;
//		break;
//	case TEE_TYPE_SM2_KEP_PUBLIC_KEY:
//		if (!IS_ENABLED(CFG_CRYPTO_SM2_KEP))
//			return TEE_ERROR_NOT_IMPLEMENTED;
//
//		s->curve = TEE_ECC_CURVE_SM2;
//		s->ops = &sm2_kep_public_key_ops;
//		break;
//	default:
//		return TEE_ERROR_NOT_IMPLEMENTED;
//	}

	s->x = crypto_bignum_allocate(key_size_bits);
	if (!s->x)
		goto err;
	s->y = crypto_bignum_allocate(key_size_bits);
	if (!s->y)
		goto err;

	return TEE_SUCCESS;

err:
	crypto_bignum_free(s->x);

	return TEE_ERROR_OUT_OF_MEMORY;
}

TEE_Result crypto_acipher_alloc_ecc_public_key(struct ecc_public_key *key,
					       uint32_t key_type,
					       size_t key_size_bits)
{
	TEE_Result res = TEE_ERROR_NOT_IMPLEMENTED;

	res = crypto_asym_alloc_ecc_public_key(key, key_type,
						   key_size_bits);
	return res;
}

void crypto_acipher_free_ecc_public_key(struct ecc_public_key *s)
{
	if (!s)
		return;

	crypto_bignum_free(s->x);
	crypto_bignum_free(s->y);
}

static int Sign(uint8_t *signature, size_t signature_size, uint32_t md_algo,
		const uint8_t *message, size_t message_size,
		const uint8_t *private_key, size_t private_key_size)
{
	int ret = 0;

	struct ecc_keypair ecc_sign_key;
	TEE_Result res;
	uint8_t hash[TEE_SHA256_HASH_SIZE] = { 0 };

	res = digest_msg(md_algo, message, message_size, hash);
	if (res)
		return res;

	// DMSG("hash of %s is :", message);
	// DHEXDUMP(hash, TEE_SHA256_HASH_SIZE);

	memset(&ecc_sign_key, 0, sizeof(ecc_sign_key));

	res = crypto_acipher_alloc_ecc_keypair(&ecc_sign_key,
					       TEE_TYPE_ECDSA_KEYPAIR, 256);
//	DMSG("crypto_acipher_alloc_ecc_keypair return %x :", res);
	if (res)
		goto out;

	ecc_sign_key.curve = TEE_ECC_CURVE_NIST_P256;

	res = crypto_bignum_bin2bn(private_key, private_key_size,
				   ecc_sign_key.d);

//	DMSG("crypto_bignum_bin2bn return %x :", res);
	if (res)
		goto out;

	size_t signature_len = signature_size;
	res = crypto_acipher_ecc_sign(TEE_ALG_ECDSA_P256, &ecc_sign_key, hash,
				      sizeof(hash), signature, &signature_len);
	if (res)
		goto out;

	ret = 1;
out:
	ecc_keypair_free(&ecc_sign_key);
//	DMSG("sign return %x :", ret);

	return ret;
}


int P256Sign(uint8_t signature[P256_SIGNATURE_SIZE], const uint8_t *message,
	     size_t message_size,
	     const uint8_t private_key[P256_PRIVATE_KEY_SIZE])
{
	return Sign(signature, P256_SIGNATURE_SIZE, MBEDTLS_MD_SHA256, message,
		    message_size, private_key, P256_PRIVATE_KEY_SIZE);
}

static int Verify(uint32_t md_algo, const uint8_t *message, size_t message_size,
		  const uint8_t *signature, size_t signature_size,
		  const uint8_t *public_key, size_t public_key_size)
{
	int ret = 0;
	struct ecc_public_key ecc_public_key;
	TEE_Result res;
	uint8_t hash[TEE_SHA256_HASH_SIZE] = { 0 };

	if (public_key_size != P256_PUBLIC_KEY_SIZE)
		return TEE_ERROR_BAD_PARAMETERS;

	res = digest_msg(md_algo, message, message_size, hash);
	if (res)
		return res;

//	DMSG("hash of %s is :", message);
//	DHEXDUMP(hash, TEE_SHA256_HASH_SIZE);

	memset(&ecc_public_key, 0, sizeof(ecc_public_key));

	res = crypto_acipher_alloc_ecc_public_key(
		&ecc_public_key, TEE_TYPE_ECDSA_PUBLIC_KEY, 256);
//	DMSG("crypto_acipher_alloc_ecc_keypair return %x :", res);
	if (res)
		goto out;
	ecc_public_key.curve = TEE_ECC_CURVE_NIST_P256;

	res = crypto_bignum_bin2bn(public_key, P256_PUBLIC_KEY_SIZE / 2,
				   ecc_public_key.x);
	if (res)
		goto out;
	res = crypto_bignum_bin2bn(&public_key[P256_PUBLIC_KEY_SIZE / 2],
				   P256_PUBLIC_KEY_SIZE / 2, ecc_public_key.y);
	if (res)
		goto out;

	res = crypto_acipher_ecc_verify(TEE_ALG_ECDSA_P256, &ecc_public_key,
					hash, sizeof(hash), signature,
					signature_size);
	if (res == TEE_SUCCESS)
		ret = 1;
out:
	crypto_acipher_free_ecc_public_key(&ecc_public_key);
	return ret;
}

int P256Verify(const uint8_t *message, size_t message_size,
	       const uint8_t signature[P256_SIGNATURE_SIZE],
	       const uint8_t public_key[P256_PUBLIC_KEY_SIZE])
{
	return Verify(MBEDTLS_MD_SHA256, message, message_size, signature,
		      P256_SIGNATURE_SIZE, public_key, P256_PUBLIC_KEY_SIZE);
}
