/*
 * SM2 Encryption alogrithm
 *
 * References:
 * - GM/T 0003-2012 Chinese National Standard:
 *      Public Key Cryptographic Algorithm SM2 Based on Elliptic Curves
 * - GM/T 0009-2012 SM2 cryptography algorithm application specification
 * - GM/T 0015-2012 Digital certificate format based on SM2 algorithm
 *
 * Thanks to MbedTLS.
 */

#if !defined(MBEDTLS_CONFIG_FILE)
#include "mbedtls/config.h"
#else
#include MBEDTLS_CONFIG_FILE
#endif

#if defined(MBEDTLS_SM2_C)
#include "mbedtls/asn1.h"
#include "mbedtls/asn1write.h"
#include "mbedtls/ecdsa.h"
#include "mbedtls/error.h"
#include "mbedtls/md_internal.h"
#include "mbedtls/sm2.h"

#if defined(MBEDTLS_PLATFORM_C)
#include "mbedtls/platform.h"
#else
#include <stdio.h>
#include <stdlib.h>
#define mbedtls_calloc calloc
#define mbedtls_printf printf
#define mbedtls_free free
#endif /* MBEDTLS_PLATFORM_C */

#if !defined(MBEDTLS_SM2_ALT)

#if !defined(MBEDTLS_SM2_CRYPT_ALT) || !defined(MBEDTLS_SM2_SIGN_ALT)

#define SM2_VALIDATE_RET(cond) MBEDTLS_INTERNAL_VALIDATE_RET(cond, MBEDTLS_ERR_ECP_BAD_INPUT_DATA)
#define SM2_VALIDATE(cond) MBEDTLS_INTERNAL_VALIDATE(cond)

/**
 * Get random r in [1, n-1]
 */
static int sm2_get_rand(mbedtls_sm2_context *ctx, mbedtls_mpi *r,
                        int (*f_rng)(void *, unsigned char *, size_t), void *p_rng)
{
    int ret;
    size_t blind_tries = 0;
    size_t nlen;
    do {
        nlen = (ctx->grp.nbits + 7) / 8;
        MBEDTLS_MPI_CHK(mbedtls_mpi_fill_random(r, nlen, f_rng, p_rng));
        MBEDTLS_MPI_CHK(mbedtls_mpi_shift_r(r, 8 * nlen - ctx->grp.nbits));

        /* See mbedtls_ecp_gen_keypair() */
        if (++blind_tries > 30) return (MBEDTLS_ERR_SM2_RANDOM_FAILED);
    } while (mbedtls_mpi_cmp_int(r, 1) < 0 || mbedtls_mpi_cmp_mpi(r, &ctx->grp.N) >= 0);
cleanup:
    return (ret);
}
#endif /* !MBEDTLS_SM2_CRYPT_ALT || !MBEDTLS_SM2_SIGN_ALT */

#if !defined(MBEDTLS_SM2_CRYPT_ALT)
/**
 * SM2 KDF (ISO/IEC 15946-2 3.1.3)
 * (GM/T 0003-2012 - Part 3: Key Exchange Protocol 5.4.3)
 */
static int mbedtls_sm2_pbkdf2(mbedtls_md_context_t *ctx, const unsigned char *password, size_t plen,
                              const unsigned char *salt, size_t slen, unsigned int iteration_count,
                              uint32_t key_length, unsigned char *output)
{
    int ret, j;
    unsigned int i;
    unsigned char md1[MBEDTLS_MD_MAX_SIZE];
    unsigned char work[MBEDTLS_MD_MAX_SIZE];
    unsigned char md_size = mbedtls_md_get_size(ctx->md_info);
    size_t use_len;
    unsigned char *out_p = output;
    unsigned char counter[4];

    memset(counter, 0, 4);
    counter[3] = 1;

    if (iteration_count > 0xFFFFFFFF) return (MBEDTLS_ERR_SM2_BAD_INPUT_DATA);

    while (key_length) {
        /* U1 ends up in work */
        if ((ret = mbedtls_md_starts(ctx)) != 0) return ret;
        if ((ret = mbedtls_md_update(ctx, password, plen)) != 0) return ret;
        if ((ret = mbedtls_md_update(ctx, salt, slen)) != 0) return ret;
        if ((ret = mbedtls_md_update(ctx, counter, 4)) != 0) return ret;
        if ((ret = mbedtls_md_finish(ctx, work)) != 0) return ret;

        memcpy(md1, work, md_size);

        for (i = 1; i < iteration_count; i++) {
            /* U2 ends up in md1 */
            if ((ret = mbedtls_md_hmac_starts(ctx, password, plen)) != 0) return (ret);
            if ((ret = mbedtls_md_hmac_update(ctx, md1, md_size)) != 0) return (ret);
            if ((ret = mbedtls_md_hmac_finish(ctx, md1)) != 0) return (ret);

            /* U1 xor U2 */
            for (j = 0; j < md_size; j++) work[j] ^= md1[j];
        }

        use_len = (key_length < md_size) ? key_length : md_size;
        memcpy(out_p, work, use_len);

        key_length -= (uint32_t)use_len;
        out_p += use_len;

        for (i = 4; i > 0; i--)
            if (++counter[i - 1] != 0) break;
    }

    return (0);
}

/*
 * Convert a Encrypt  (given by context) to raw plain data:
 * c1_x c1_y digest||enc_data =>
 * 04||C1||C3(digest)||C2(enc_data)
 */
static int mbedtls_sm2_encrypt_data_to_raw(const mbedtls_md_info_t *md_info,
                                           const mbedtls_mpi *c1_x, const mbedtls_mpi *c1_y,
                                           const unsigned char *hash_with_enc_buf,
                                           size_t hash_with_enc_len, unsigned char *out_buf,
                                           size_t *slen)
{
    int ret = 0;

    (void)md_info;
    // 04
    out_buf[0] = MBEDTLS_ECP_POINT_CONVERSION_UNCOMPRESSED;
    *slen = 1;
    // C1
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(c1_x, out_buf + *slen, mbedtls_mpi_size(c1_x)));
    *slen += mbedtls_mpi_size(c1_x);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(c1_y, out_buf + *slen, mbedtls_mpi_size(c1_y)));
    *slen += mbedtls_mpi_size(c1_y);

    // C3||C2
    memcpy(out_buf + *slen, hash_with_enc_buf, hash_with_enc_len);
    *slen += hash_with_enc_len;

cleanup:
    return (0);
}

/*
 * Convert a signature (given by context) to ASN.1
 */
static int sm2_encrypt_data_to_asn1(const mbedtls_md_info_t *md_info, const mbedtls_mpi *c1_x,
                                    const mbedtls_mpi *c1_y, const unsigned char *hash_with_enc_buf,
                                    size_t hash_with_enc_len, unsigned char *out_buf, size_t *slen)
{
    int ret;
    unsigned char buf[MBEDTLS_ECDSA_MAX_LEN];
    unsigned char *p = buf + sizeof(buf);
    size_t len = 0;

    // check input length
    if (hash_with_enc_len <= md_info->size) { return MBEDTLS_ERR_SM2_BAD_INPUT_DATA; }

    // octet string enc data
    MBEDTLS_ASN1_CHK_ADD(len,
                         mbedtls_asn1_write_octet_string(&p, buf, hash_with_enc_buf + md_info->size,
                                                         hash_with_enc_len - md_info->size));

    // octet string digest data
    MBEDTLS_ASN1_CHK_ADD(
        len, mbedtls_asn1_write_octet_string(&p, buf, hash_with_enc_buf, md_info->size));
    // interger
    MBEDTLS_ASN1_CHK_ADD(len, mbedtls_asn1_write_mpi(&p, buf, c1_y));
    // integer
    MBEDTLS_ASN1_CHK_ADD(len, mbedtls_asn1_write_mpi(&p, buf, c1_x));

    // sequence
    MBEDTLS_ASN1_CHK_ADD(len, mbedtls_asn1_write_len(&p, buf, len));
    MBEDTLS_ASN1_CHK_ADD(
        len, mbedtls_asn1_write_tag(&p, buf, MBEDTLS_ASN1_CONSTRUCTED | MBEDTLS_ASN1_SEQUENCE));

    memcpy(out_buf, p, len);
    *slen = len;

    return (0);
}

/*
 * encrypt with sm2
 * C1 -> c1_x c1_y
 * C3||C2 -> c2c3
 * input: input plain data
 * ilen: length of input
 * c1_x c1_y: encrypt data part C1
 * c2c3: encrypt data part C3||C2
 */
static int mbedtls_sm2_encrypt_internal(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                                        mbedtls_mpi *c1_x, mbedtls_mpi *c1_y,
                                        const unsigned char *input, size_t ilen,
                                        unsigned char *c2c3, size_t *c2c3_len,
                                        int (*f_rng)(void *, unsigned char *, size_t), void *p_rng)
{
    int ret = 0;
    size_t i;
    mbedtls_mpi k;
    mbedtls_mpi h;
    mbedtls_ecp_point point;
    mbedtls_md_context_t md_ctx;
    size_t xlen, ylen;
    unsigned char *xym = NULL;
    size_t md_size;

    mbedtls_mpi_init(&k);
    mbedtls_mpi_init(&h);
    mbedtls_md_init(&md_ctx);
    mbedtls_ecp_point_init(&point);
    MBEDTLS_MPI_CHK(mbedtls_md_setup(&md_ctx, mbedtls_md_info_from_type(md_alg), 0));

    do {
        *c2c3_len = 0;

        /* A1: rand k in [1, n-1] */
        MBEDTLS_MPI_CHK(sm2_get_rand(ctx, &k, f_rng, p_rng));

        /* A2: C1 = [k]G = (x1, y1) */
        MBEDTLS_MPI_CHK(mbedtls_ecp_mul(&ctx->grp, &point, &k, &ctx->grp.G, NULL, NULL));

        // copy C1 to c1_x||c1_y
        MBEDTLS_MPI_CHK(mbedtls_mpi_copy(c1_x, &point.X));
        MBEDTLS_MPI_CHK(mbedtls_mpi_copy(c1_y, &point.Y));

        /* A3: check [h]P != O */
        MBEDTLS_MPI_CHK(mbedtls_mpi_lset(&h, 1));
        MBEDTLS_MPI_CHK(mbedtls_ecp_mul(&ctx->grp, &point, &h, &ctx->Q, NULL, NULL));
        MBEDTLS_MPI_CHK(mbedtls_ecp_is_zero(&point));

        /* A4: [k]P = (x2, y2) */
        MBEDTLS_MPI_CHK(mbedtls_ecp_mul(&ctx->grp, &point, &k, &ctx->Q, NULL, NULL));

        /* A5: t = KDF(x2 || y2, klen) */
        xlen = mbedtls_mpi_size(&point.X);
        ylen = mbedtls_mpi_size(&point.Y);
        if ((xym = mbedtls_calloc(1, xlen + ylen + ilen)) == NULL) {
            MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_ALLOC_FAILED);
        }
        MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&point.X, xym, xlen));
        MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&point.Y, xym + xlen, ylen));
        MBEDTLS_MPI_CHK(
            mbedtls_sm2_pbkdf2(&md_ctx, xym, xlen + ylen, NULL, 0, 0, ilen, xym + xlen + ylen));
        for (i = 0; i < ilen; i++) {
            if (*(xym + xlen + ylen + i)) { break; }
        }
        if (i >= xlen + ylen) { continue; }

        break;
    } while (0);

    // C1 || C3 || C2

    /* A6: C2 = M xor t */
    // copy c2
    md_size = mbedtls_md_get_size(md_ctx.md_info);
    for (i = 0; i < ilen; i++) { c2c3[i + md_size] = input[i] ^ *(xym + xlen + ylen + i); }

    /* A7: C3 = Hash(x2 || M || y2) */
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&point.X, xym, xlen));
    memmove(xym + xlen, input, ilen);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&point.Y, xym + xlen + ilen, ylen));
    MBEDTLS_MPI_CHK(mbedtls_md(md_ctx.md_info, xym, xlen + ilen + ylen, c2c3));
    *c2c3_len = ilen + md_size;

cleanup:
    mbedtls_mpi_free(&k);
    mbedtls_mpi_free(&h);
    mbedtls_ecp_point_free(&point);
    if (xym) { mbedtls_free(xym); }
    mbedtls_md_free(&md_ctx);

    return (ret);
}

/*
 * decrypt with sm2
 * C1: the C1 part of encrypted data
 * hash: the C3 part of encrypted data (digest)
 * enc: the C2 part of encrypted data
 * output: the decrypt data
 */
static int mbedtls_sm2_decrypt_internal(mbedtls_sm2_context *ctx, mbedtls_md_context_t *md_ctx,
                                        mbedtls_ecp_point *C1, const unsigned char *hash,
                                        size_t hlen, const unsigned char *enc, size_t enclen,
                                        unsigned char *output, size_t *olen)
{
    int ret = 0;
    size_t i;
    mbedtls_mpi h;
    mbedtls_ecp_point point;
    size_t xlen, ylen;
    unsigned char *xym = NULL;

    // check hlen if the digest length
    if (hlen != mbedtls_md_get_size(md_ctx->md_info)) { return MBEDTLS_ERR_SM2_BAD_INPUT_DATA; }

    mbedtls_ecp_point_init(&point);
    mbedtls_mpi_init(&h);

    /* B1: get C1 */
    // c1len =  1 + (ctx->grp.nbits + 7) / 8 * 2;

    /* B2: check [h]C1 != O */
    MBEDTLS_MPI_CHK(mbedtls_mpi_lset(&h, 1));
    MBEDTLS_MPI_CHK(mbedtls_ecp_mul(&ctx->grp, &point, &h, C1, NULL, NULL));
    MBEDTLS_MPI_CHK(mbedtls_ecp_is_zero(&point));

    /* B3: [d]C1 = (x2, y2) */
    MBEDTLS_MPI_CHK(mbedtls_ecp_mul(&ctx->grp, &point, &ctx->d, C1, NULL, NULL));

    /* B4: t = KDF(x2 || y2, klen) */
    xlen = mbedtls_mpi_size(&point.X);
    ylen = mbedtls_mpi_size(&point.Y);
    if ((xym = mbedtls_calloc(1, xlen + ylen + enclen)) == NULL) {
        MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_ALLOC_FAILED);
    }
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&point.X, xym, xlen));
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&point.Y, xym + xlen, ylen));
    MBEDTLS_MPI_CHK(
        mbedtls_sm2_pbkdf2(md_ctx, xym, xlen + ylen, NULL, 0, 0, enclen, xym + xlen + ylen));
    for (i = 0; i < enclen; i++) {
        if (*(xym + xlen + ylen + i)) { break; }
    }
    if (i >= xlen + ylen) { MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_KDF_FAILED); }

    /* B5: M' = C2 xor t */
    for (i = 0; i < enclen; i++) { output[i] = enc[i] ^ *(xym + xlen + ylen + i); }
    *olen = enclen;

    /* B6: check Hash(x2 || M' || y2) == C3 */
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&point.X, xym, xlen));
    memmove(xym + xlen, output, *olen);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&point.Y, xym + xlen + *olen, ylen));
    MBEDTLS_MPI_CHK(mbedtls_md(md_ctx->md_info, xym, xlen + *olen + ylen, xym));
    if (memcmp(hash, xym, hlen)) { MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_DECRYPT_BAD_HASH); }

cleanup:
    mbedtls_mpi_free(&h);
    mbedtls_ecp_point_free(&point);
    if (xym) { mbedtls_free(xym); }

    return (ret);
}

/*
 * sm2 encrypt, output using write_fun call back function
 */
static int mbedtls_sm2_encrypt_wrap(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                                    const unsigned char *input, size_t ilen, unsigned char *output,
                                    size_t *olen, sm2_write_encrypt_data write_fun,
                                    int (*f_rng)(void *, unsigned char *, size_t), void *p_rng)
{
    int ret;
    mbedtls_mpi c1_x, c1_y;
    size_t md_size;
    mbedtls_md_context_t md_ctx;
    unsigned char *hash_with_enc_buf = NULL;
    size_t hash_with_enc_len = 0;

    mbedtls_mpi_init(&c1_x);
    mbedtls_mpi_init(&c1_y);

    MBEDTLS_MPI_CHK(mbedtls_md_setup(&md_ctx, mbedtls_md_info_from_type(md_alg), 0));

    md_size = mbedtls_md_get_size(md_ctx.md_info);

    if ((hash_with_enc_buf = mbedtls_calloc(1, md_size + ilen)) == NULL) {
        MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_ALLOC_FAILED);
    }

    MBEDTLS_MPI_CHK(mbedtls_sm2_encrypt_internal(ctx, md_alg, &c1_x, &c1_y, input, ilen,
                                                 hash_with_enc_buf, &hash_with_enc_len, f_rng,
                                                 p_rng));

    if (write_fun) {
        write_fun(md_ctx.md_info, &c1_x, &c1_y, hash_with_enc_buf, hash_with_enc_len, output, olen);
    }

cleanup:
    mbedtls_mpi_free(&c1_x);
    mbedtls_mpi_free(&c1_y);
    if (hash_with_enc_buf) { mbedtls_free(hash_with_enc_buf); }

    return (ret);
}

int mbedtls_sm2_encrypt_raw(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                            const unsigned char *input, size_t ilen, unsigned char *output,
                            size_t *olen, int (*f_rng)(void *, unsigned char *, size_t),
                            void *p_rng)
{
    return mbedtls_sm2_encrypt_wrap(ctx, md_alg, input, ilen, output, olen,
                                    mbedtls_sm2_encrypt_data_to_raw, f_rng, p_rng);
}

int mbedtls_sm2_encrypt_asn1(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                             const unsigned char *input, size_t ilen, unsigned char *output,
                             size_t *olen, int (*f_rng)(void *, unsigned char *, size_t),
                             void *p_rng)
{
    return mbedtls_sm2_encrypt_wrap(ctx, md_alg, input, ilen, output, olen,
                                    sm2_encrypt_data_to_asn1, f_rng, p_rng);
}

/*
 * sm2 decrypt data to raw: 04||C1||C3||C2
 */
int mbedtls_sm2_decrypt_raw(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                            const unsigned char *input, size_t ilen, unsigned char *output,
                            size_t *olen)
{
    int ret = 0;
    mbedtls_ecp_point C1;
    mbedtls_md_context_t md_ctx;
    size_t c1len;
    size_t mdlen;

    mbedtls_ecp_point_init(&C1);
    mbedtls_md_init(&md_ctx);

    MBEDTLS_MPI_CHK(mbedtls_md_setup(&md_ctx, mbedtls_md_info_from_type(md_alg), 0));

    mdlen = mbedtls_md_get_size(md_ctx.md_info);
    c1len = 1 + (ctx->grp.nbits + 7) / 8 * 2;
    if (ilen <= mdlen + c1len) {
        ret = MBEDTLS_ERR_SM2_BAD_INPUT_DATA;
        goto cleanup;
    }

    MBEDTLS_MPI_CHK(mbedtls_ecp_point_read_binary(&ctx->grp, &C1, input, c1len));

    ret = mbedtls_sm2_decrypt_internal(ctx, &md_ctx, &C1, input + c1len, mdlen,
                                       input + c1len + mdlen, ilen - c1len - mdlen, output, olen);
cleanup:
    mbedtls_ecp_point_free(&C1);
    mbedtls_md_free(&md_ctx);
    return (ret);
}

int mbedtls_sm2_decrypt_asn1(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                             const unsigned char *input, size_t ilen, unsigned char *output,
                             size_t *olen)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
    unsigned char *p = (unsigned char *)input;
    const unsigned char *end = input + ilen;
    size_t len;
    mbedtls_ecp_point C1;
    unsigned char *hash_buf = NULL;
    size_t hash_len;
    unsigned char *enc_buf = NULL;
    mbedtls_md_context_t md_ctx;

    SM2_VALIDATE_RET(ctx != NULL);
    SM2_VALIDATE_RET(output != NULL);
    SM2_VALIDATE_RET(input != NULL);

    mbedtls_ecp_point_init(&C1);
    mbedtls_md_init(&md_ctx);

    MBEDTLS_MPI_CHK(mbedtls_md_setup(&md_ctx, mbedtls_md_info_from_type(md_alg), 0));

    // sequence
    if ((ret =
             mbedtls_asn1_get_tag(&p, end, &len, MBEDTLS_ASN1_CONSTRUCTED | MBEDTLS_ASN1_SEQUENCE))
        != 0) {
        ret += MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
        goto cleanup;
    }

    if (p + len != end) {
        ret = MBEDTLS_ERROR_ADD(MBEDTLS_ERR_ECP_BAD_INPUT_DATA, MBEDTLS_ERR_ASN1_LENGTH_MISMATCH);
        goto cleanup;
    }

    // integer C1 (point)
    if ((ret = mbedtls_asn1_get_mpi(&p, end, &C1.X)) != 0
        || (ret = mbedtls_asn1_get_mpi(&p, end, &C1.Y)) != 0) {
        ret += MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
        goto cleanup;
    }
    MBEDTLS_MPI_CHK(mbedtls_mpi_lset(&C1.Z, 1));

    // octet string: digest
    if ((ret = mbedtls_asn1_get_tag(&p, end, &hash_len, MBEDTLS_ASN1_OCTET_STRING)) != 0) {
        ret += MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
        goto cleanup;
    }
    hash_buf = p;
    p += hash_len;

    // octet string: enc data
    if ((ret = mbedtls_asn1_get_tag(&p, end, &len, MBEDTLS_ASN1_OCTET_STRING)) != 0) {
        ret += MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
        goto cleanup;
    }
    enc_buf = p;
    p += len;

    // decrypt
    ret = mbedtls_sm2_decrypt_internal(ctx, &md_ctx, &C1, hash_buf, hash_len, enc_buf, len, output,
                                       olen);

    /* At this point we know that the buffer starts with a valid signature.
     * Return 0 if the buffer just contains the signature, and a specific
     * error code if the valid signature is followed by more data. */
    if (p != end) { ret = MBEDTLS_ERR_ECP_SIG_LEN_MISMATCH; }

cleanup:
    mbedtls_ecp_point_free(&C1);
    mbedtls_md_free(&md_ctx);

    return ret;
}
#endif /* !MBEDTLS_SM2_CRYPT_ALT */

#if !defined(MBEDTLS_SM2_SIGN_ALT)
/*
 * sm2 signature,
 * output to mpi r_in and s_in
 */
static int mbedtls_sm2_sign_internal(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                                     mbedtls_mpi *r_in, mbedtls_mpi *s_in,
                                     const unsigned char *hash,
                                     int (*f_rng)(void *, unsigned char *, size_t), void *p_rng)
{
    int ret = 0;
    mbedtls_mpi k;
    mbedtls_mpi e;
    mbedtls_mpi r;
    mbedtls_mpi s;
    mbedtls_ecp_point point;

    mbedtls_mpi_init(&e);
    mbedtls_mpi_init(&k);
    mbedtls_mpi_init(&r);
    mbedtls_mpi_init(&s);
    mbedtls_ecp_point_init(&point);

    /**
     * A1: M' = Z || M
     * A2: e = Hash(M')
     * Parameter <hash> is the digest of <M'>, need convert to bignum <e>.
     */
    do {
        /* A3: rand k in [1, n-1] */
        MBEDTLS_MPI_CHK(sm2_get_rand(ctx, &k, f_rng, p_rng));

        /* A4: (x1, y1) = [k]G */
        MBEDTLS_MPI_CHK(mbedtls_ecp_mul(&ctx->grp, &point, &k, &ctx->grp.G, NULL, NULL));

        /* A5: r = (e + x1) mod n; if (r == 0 || r + k == n) goto A3; */
        MBEDTLS_MPI_CHK(mbedtls_mpi_read_binary(
            &e, hash, mbedtls_md_get_size(mbedtls_md_info_from_type(md_alg))));
        MBEDTLS_MPI_CHK(mbedtls_mpi_add_mpi(&r, &e, &point.X));
        MBEDTLS_MPI_CHK(mbedtls_mpi_mod_mpi(&r, &r, &ctx->grp.N));
        MBEDTLS_MPI_CHK(mbedtls_mpi_add_mpi(&s, &r, &k));
        if (mbedtls_mpi_cmp_int(&r, 0) == 0 || mbedtls_mpi_cmp_mpi(&s, &ctx->grp.N) == 0) {
            continue;
        }
        // MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&r, sig,
        //             mbedtls_mpi_size(&r)));
        MBEDTLS_MPI_CHK(mbedtls_mpi_copy(r_in, &r));

        /* A6: s = (((1 + d)^-1) * (k - r * d)) mod n; if (s == 0) goto A3; */
        MBEDTLS_MPI_CHK(mbedtls_mpi_mul_mpi(&r, &r, &ctx->d));
        MBEDTLS_MPI_CHK(mbedtls_mpi_sub_mpi(&r, &k, &r));
        MBEDTLS_MPI_CHK(mbedtls_mpi_add_int(&s, &ctx->d, 1));
        MBEDTLS_MPI_CHK(mbedtls_mpi_inv_mod(&s, &s, &ctx->grp.N));
        MBEDTLS_MPI_CHK(mbedtls_mpi_mul_mpi(&s, &s, &r));
        MBEDTLS_MPI_CHK(mbedtls_mpi_mod_mpi(&s, &s, &ctx->grp.N));
        if (mbedtls_mpi_cmp_int(&s, 0) == 0) { continue; }

        break;
    } while (1);

    // MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&s, sig + (ctx->grp.nbits + 7) /
    // 8,
    //             mbedtls_mpi_size(&s)));
    MBEDTLS_MPI_CHK(mbedtls_mpi_copy(s_in, &s));
cleanup:
    mbedtls_mpi_free(&k);
    mbedtls_mpi_free(&e);
    mbedtls_mpi_free(&r);
    mbedtls_mpi_free(&s);
    mbedtls_ecp_point_free(&point);

    return (ret);
}

typedef int (*sm2_signature_write)(mbedtls_sm2_context *ctx, const mbedtls_mpi *r,
                                   const mbedtls_mpi *s, unsigned char *sig, size_t *slen);

/*
 * Convert a signature (given by context) to ASN.1
 */
static int sm2_signature_to_asn1(mbedtls_sm2_context *ctx, const mbedtls_mpi *r,
                                 const mbedtls_mpi *s, unsigned char *sig, size_t *slen)
{
    int ret;
    unsigned char buf[MBEDTLS_ECDSA_MAX_LEN];
    unsigned char *p = buf + sizeof(buf);
    size_t len = 0;

    (void)ctx;
    MBEDTLS_ASN1_CHK_ADD(len, mbedtls_asn1_write_mpi(&p, buf, s));
    MBEDTLS_ASN1_CHK_ADD(len, mbedtls_asn1_write_mpi(&p, buf, r));

    MBEDTLS_ASN1_CHK_ADD(len, mbedtls_asn1_write_len(&p, buf, len));
    MBEDTLS_ASN1_CHK_ADD(
        len, mbedtls_asn1_write_tag(&p, buf, MBEDTLS_ASN1_CONSTRUCTED | MBEDTLS_ASN1_SEQUENCE));

    memcpy(sig, p, len);
    *slen = len;

    return (0);
}

/*
 * Convert a signature (given by context) to ASN.1
 */
static int sm2_signature_to_raw(mbedtls_sm2_context *ctx, const mbedtls_mpi *r,
                                const mbedtls_mpi *s, unsigned char *sig, size_t *slen)
{
    int ret;

    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(r, sig, mbedtls_mpi_size(r)));
    MBEDTLS_MPI_CHK(
        mbedtls_mpi_write_binary(s, sig + (ctx->grp.nbits + 7) / 8, mbedtls_mpi_size(s)));
    *slen = ((ctx->grp.nbits + 7) / 8) * 2;

cleanup:
    return (0);
}

/*
 * Compute and write signature
 */
static int mbedtls_sm2_sign_wrap(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                                 const unsigned char *hash, unsigned char *sig, size_t *slen,
                                 sm2_signature_write write_fun,
                                 int (*f_rng)(void *, unsigned char *, size_t), void *p_rng)
{
    int ret;
    mbedtls_mpi r, s;

    mbedtls_mpi_init(&r);
    mbedtls_mpi_init(&s);

    MBEDTLS_MPI_CHK(
        mbedtls_sm2_sign_internal((mbedtls_sm2_context *)ctx, md_alg, &r, &s, hash, f_rng, p_rng));

    if (write_fun) { MBEDTLS_MPI_CHK(write_fun((mbedtls_sm2_context *)ctx, &r, &s, sig, slen)); }

cleanup:
    mbedtls_mpi_free(&r);
    mbedtls_mpi_free(&s);

    return (ret);
}

int mbedtls_sm2_sign_raw(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                         const unsigned char *hash, unsigned char *sig, size_t *slen,
                         int (*f_rng)(void *, unsigned char *, size_t), void *p_rng)
{
    return mbedtls_sm2_sign_wrap(ctx, md_alg, hash, sig, slen, sm2_signature_to_raw, f_rng, p_rng);
}

int mbedtls_sm2_sign_asn1(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                          const unsigned char *hash, unsigned char *sig, size_t *slen,
                          int (*f_rng)(void *, unsigned char *, size_t), void *p_rng)
{
    return mbedtls_sm2_sign_wrap(ctx, md_alg, hash, sig, slen, sm2_signature_to_asn1, f_rng, p_rng);
}

static int mbedtls_sm2_verify_internal(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                                       const mbedtls_mpi *r, const mbedtls_mpi *s,
                                       const unsigned char *hash, size_t hlen)
{
    int ret = 0;
    mbedtls_mpi e;
    mbedtls_mpi t;
    mbedtls_ecp_point point;

    if (hlen != mbedtls_md_get_size(mbedtls_md_info_from_type(md_alg))) {
        return MBEDTLS_ERR_SM2_BAD_INPUT_DATA;
    }

    mbedtls_mpi_init(&e);
    mbedtls_mpi_init(&t);
    mbedtls_ecp_point_init(&point);

    /* B1,B2: check r, s in [1, n-1] */
    if (mbedtls_mpi_cmp_int(r, 1) < 0 || mbedtls_mpi_cmp_mpi(r, &ctx->grp.N) >= 0
        || mbedtls_mpi_cmp_int(s, 1) < 0 || mbedtls_mpi_cmp_mpi(s, &ctx->grp.N) >= 0) {
        MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_BAD_SIGNATURE - 1);
    }

    /**
     * B3: M' = Z || M
     * B4: e' = Hash(M')
     * Parameter <hash> is the digest of <M'>, need convert to bignum <e>.
     */
    MBEDTLS_MPI_CHK(mbedtls_mpi_read_binary(&e, hash, hlen));

    /* B5: t = (r + s) mod n; if (t == 0) return error; */
    MBEDTLS_MPI_CHK(mbedtls_mpi_add_mpi(&t, r, s));
    MBEDTLS_MPI_CHK(mbedtls_mpi_mod_mpi(&t, &t, &ctx->grp.N));
    if (mbedtls_mpi_cmp_int(&t, 0) == 0) { MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_BAD_SIGNATURE - 2); }

    /* B6: (x1, y1) = [s]G + [t]P */
    MBEDTLS_MPI_CHK(mbedtls_ecp_muladd(&ctx->grp, &point, s, &ctx->grp.G, &t, &ctx->Q));

    /* B7: R = (e + x1) mod n; if (R == r) Success; else Failed; */
    mbedtls_mpi_free(&t);
    mbedtls_mpi_init(&t);
    MBEDTLS_MPI_CHK(mbedtls_mpi_add_mpi(&t, &e, &point.X));
    MBEDTLS_MPI_CHK(mbedtls_mpi_mod_mpi(&t, &t, &ctx->grp.N));
    if (mbedtls_mpi_cmp_mpi(&t, r) != 0) { MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_BAD_SIGNATURE - 3); }

cleanup:
    mbedtls_mpi_free(&e);
    mbedtls_mpi_free(&t);
    mbedtls_ecp_point_free(&point);

    return (ret);
}

/*
 * Read and check signature
 * asn.1
 */
int mbedtls_sm2_verify_asn1(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                            const unsigned char *hash, size_t hlen, const unsigned char *sig,
                            size_t slen)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
    unsigned char *p = (unsigned char *)sig;
    const unsigned char *end = sig + slen;
    size_t len;
    mbedtls_mpi r, s;
    SM2_VALIDATE_RET(ctx != NULL);
    SM2_VALIDATE_RET(hash != NULL);
    SM2_VALIDATE_RET(sig != NULL);

    mbedtls_mpi_init(&r);
    mbedtls_mpi_init(&s);

    if ((ret =
             mbedtls_asn1_get_tag(&p, end, &len, MBEDTLS_ASN1_CONSTRUCTED | MBEDTLS_ASN1_SEQUENCE))
        != 0) {
        ret += MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
        goto cleanup;
    }

    if (p + len != end) {
        ret = MBEDTLS_ERROR_ADD(MBEDTLS_ERR_ECP_BAD_INPUT_DATA, MBEDTLS_ERR_ASN1_LENGTH_MISMATCH);
        goto cleanup;
    }

    if ((ret = mbedtls_asn1_get_mpi(&p, end, &r)) != 0
        || (ret = mbedtls_asn1_get_mpi(&p, end, &s)) != 0) {
        ret += MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
        goto cleanup;
    }

    if ((ret = mbedtls_sm2_verify_internal(ctx, md_alg, &r, &s, hash, hlen)) != 0) { goto cleanup; }

    /* At this point we know that the buffer starts with a valid signature.
     * Return 0 if the buffer just contains the signature, and a specific
     * error code if the valid signature is followed by more data. */
    if (p != end) { ret = MBEDTLS_ERR_ECP_SIG_LEN_MISMATCH; }

cleanup:
    mbedtls_mpi_free(&r);
    mbedtls_mpi_free(&s);

    return ret;
}

// verify with raw signature 64 bytes data
int mbedtls_sm2_verify_raw(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg,
                                  const unsigned char *hash, size_t hlen, const unsigned char *sig)
{
    int ret = 0;
    mbedtls_mpi r;
    mbedtls_mpi s;
    mbedtls_mpi_init(&r);
    mbedtls_mpi_init(&s);

    /* B1,B2: check r, s in [1, n-1] */
    MBEDTLS_MPI_CHK(mbedtls_mpi_read_binary(&r, sig, (ctx->grp.nbits + 7) / 8));
    MBEDTLS_MPI_CHK(
        mbedtls_mpi_read_binary(&s, sig + (ctx->grp.nbits + 7) / 8, (ctx->grp.nbits + 7) / 8));

    ret = mbedtls_sm2_verify_internal(ctx, md_alg, &r, &s, hash, hlen);
cleanup:
    mbedtls_mpi_free(&r);
    mbedtls_mpi_free(&s);
    printf("verify with %d", ret);
    return (ret);
}

int mbedtls_sm2_hash_z(mbedtls_sm2_context *ctx, mbedtls_md_type_t md_alg, const char *id,
                       size_t idlen, unsigned char *z)
{
    int ret = 0;
    unsigned char *m = NULL;
    unsigned char *p;
    size_t mlen;
    size_t l;
    const char *def_id = MBEDTLS_SM2_GMT09_DEFAULT_ID;
    size_t def_id_len = strlen(def_id);
    const mbedtls_md_info_t *md_info = NULL;

    if (id != NULL) {
        def_id = (char *)id;
        def_id_len = idlen;
    }
    md_info = mbedtls_md_info_from_type(md_alg);
    if (md_info == NULL) { MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_BAD_INPUT_DATA); }
    mlen = 2 + def_id_len + mbedtls_mpi_size(&ctx->grp.A) + mbedtls_mpi_size(&ctx->grp.B)
           + mbedtls_mpi_size(&ctx->grp.G.X) + mbedtls_mpi_size(&ctx->grp.G.Y)
           + mbedtls_mpi_size(&ctx->Q.X) + mbedtls_mpi_size(&ctx->Q.Y);
    if ((m = mbedtls_calloc(1, mlen)) == NULL) { MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_ALLOC_FAILED); }

    m[0] = (def_id_len >> 5) & 0xFF;
    m[1] = (def_id_len << 3) & 0xFF;
    p = m + 2;
    memmove(p, def_id, def_id_len);
    p += def_id_len;
    l = mbedtls_mpi_size(&ctx->grp.A);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&ctx->grp.A, p, l));
    p += l;
    l = mbedtls_mpi_size(&ctx->grp.B);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&ctx->grp.B, p, l));
    p += l;
    l = mbedtls_mpi_size(&ctx->grp.G.X);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&ctx->grp.G.X, p, l));
    p += l;
    l = mbedtls_mpi_size(&ctx->grp.G.Y);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&ctx->grp.G.Y, p, l));
    p += l;
    l = mbedtls_mpi_size(&ctx->Q.X);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&ctx->Q.X, p, l));
    p += l;
    l = mbedtls_mpi_size(&ctx->Q.Y);
    MBEDTLS_MPI_CHK(mbedtls_mpi_write_binary(&ctx->Q.Y, p, l));
    p += l;
    MBEDTLS_MPI_CHK(mbedtls_md(md_info, m, p - m, z));

cleanup:
    if (m) { mbedtls_free(m); }

    return (ret);
}

int mbedtls_sm2_hash_e(mbedtls_md_type_t md_alg, const unsigned char *z, const unsigned char *input,
                       size_t ilen, unsigned char *e)
{
    int ret = 0;
    const mbedtls_md_info_t *md_info = NULL;
    mbedtls_md_context_t md_ctx;

    md_info = mbedtls_md_info_from_type(md_alg);
    if (md_info == NULL) MBEDTLS_MPI_CHK(MBEDTLS_ERR_SM2_BAD_INPUT_DATA);

    mbedtls_md_init(&md_ctx);
    if ((ret = mbedtls_md_setup(&md_ctx, md_info, 0)) != 0) goto cleanup;

    if ((ret = mbedtls_md_starts(&md_ctx)) != 0) return ret;
    if ((ret = mbedtls_md_update(&md_ctx, z, mbedtls_md_get_size(md_info))) != 0) return ret;
    if ((ret = mbedtls_md_update(&md_ctx, input, ilen)) != 0) return ret;
    if ((ret = mbedtls_md_finish(&md_ctx, e)) != 0) return ret;

cleanup:
    mbedtls_md_free(&md_ctx);

    return (ret);
}

int mbedtls_md_sm2(mbedtls_sm2_context *key_ctx, mbedtls_md_type_t md_alg,
                   const unsigned char *input, size_t ilen, unsigned char *output)
{
    int ret = 0;
    unsigned char z[MBEDTLS_MD_MAX_SIZE];

    if ((ret = mbedtls_sm2_hash_z(key_ctx, md_alg, NULL, 0, z)) != 0) {
        return (ret);
    }
    if ((ret = mbedtls_sm2_hash_e(md_alg, z, input, ilen, output)) != 0) {
        return (ret);
    }

    return 0;
}

#endif /* !MBEDTLS_SM2_SIGN_ALT */

#if !defined(MBEDTLS_SM2_GENKEY_ALT)
/*
 * Generate key pair
 */
int mbedtls_sm2_genkey(mbedtls_sm2_context *ctx, mbedtls_ecp_group_id gid,
                       int (*f_rng)(void *, unsigned char *, size_t), void *p_rng)
{
    return (mbedtls_ecp_group_load(&ctx->grp, gid)
            || mbedtls_ecp_gen_keypair(&ctx->grp, &ctx->d, &ctx->Q, f_rng, p_rng));
}
#endif /* !MBEDTLS_SM2_GENKEY_ALT */

int mbedtls_sm2_from_keypair(mbedtls_sm2_context *ctx, const mbedtls_ecp_keypair *key)
{
    int ret;

    if ((ret = mbedtls_ecp_group_copy(&ctx->grp, &key->grp)) != 0
        || (ret = mbedtls_mpi_copy(&ctx->d, &key->d)) != 0
        || (ret = mbedtls_ecp_copy(&ctx->Q, &key->Q)) != 0) {
        mbedtls_sm2_free(ctx);
    }

    return (ret);
}

void mbedtls_sm2_init(mbedtls_sm2_context *ctx)
{
    mbedtls_ecp_keypair_init(ctx);
}

void mbedtls_sm2_free(mbedtls_sm2_context *ctx)
{
    mbedtls_ecp_keypair_free(ctx);
}

#endif /* !MBEDTLS_SM2_ALT */

#if defined(MBEDTLS_SELF_TEST)

/*
 * SM2 test vectors from: GM/T 0003-2012 Chinese National Standard
 */
static const unsigned char sm2_test_plaintext[] = {
    /* "encryption standard" */
    0x65, 0x6E, 0x63, 0x72, 0x79, 0x70, 0x74, 0x69, 0x6F, 0x6E,
    0x20, 0x73, 0x74, 0x61, 0x6E, 0x64, 0x61, 0x72, 0x64,
};

static const unsigned char sm2_test_messagetext[] = {
    /* message digest */
    0x6D, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x20, 0x64, 0x69, 0x67, 0x65, 0x73, 0x74,
};

static const char *sm2_enc_prikey =
    "3945208F7B2144B13F36E38AC6D39F95889393692860B51A42FB81EF4DF7C5B8";
static const char *sm2_enc_pubkey_x =
    "09F9DF311E5421A150DD7D161E4BC5C672179FAD1833FC076BB08FF356F35020";
static const char *sm2_enc_pubkey_y =
    "CCEA490CE26775A52DC6EA718CC1AA600AED05FBF35E084A6632F6072DA9AD13";

static const unsigned char sm2_enc_rand_k[] = {
    0x59, 0x27, 0x6E, 0x27, 0xD5, 0x06, 0x86, 0x1A, 0x16, 0x68, 0x0F, 0x3A, 0xD9, 0xC0, 0x2D, 0xCC,
    0xEF, 0x3C, 0xC1, 0xFA, 0x3C, 0xDB, 0xE4, 0xCE, 0x6D, 0x54, 0xB8, 0x0D, 0xEA, 0xC1, 0xBC, 0x21,
};

static const unsigned char sm2_enc_ciphertext[] = {
    0x04,

    0x04, 0xEB, 0xFC, 0x71, 0x8E, 0x8D, 0x17, 0x98, 0x62, 0x04, 0x32, 0x26, 0x8E, 0x77, 0xFE, 0xB6,
    0x41, 0x5E, 0x2E, 0xDE, 0x0E, 0x07, 0x3C, 0x0F, 0x4F, 0x64, 0x0E, 0xCD, 0x2E, 0x14, 0x9A, 0x73,

    0xE8, 0x58, 0xF9, 0xD8, 0x1E, 0x54, 0x30, 0xA5, 0x7B, 0x36, 0xDA, 0xAB, 0x8F, 0x95, 0x0A, 0x3C,
    0x64, 0xE6, 0xEE, 0x6A, 0x63, 0x09, 0x4D, 0x99, 0x28, 0x3A, 0xFF, 0x76, 0x7E, 0x12, 0x4D, 0xF0,

    0x59, 0x98, 0x3C, 0x18, 0xF8, 0x09, 0xE2, 0x62, 0x92, 0x3C, 0x53, 0xAE, 0xC2, 0x95, 0xD3, 0x03,
    0x83, 0xB5, 0x4E, 0x39, 0xD6, 0x09, 0xD1, 0x60, 0xAF, 0xCB, 0x19, 0x08, 0xD0, 0xBD, 0x87, 0x66,

    0x21, 0x88, 0x6C, 0xA9, 0x89, 0xCA, 0x9C, 0x7D, 0x58, 0x08, 0x73, 0x07, 0xCA, 0x93, 0x09, 0x2D,
    0x65, 0x1E, 0xFA};

static const char *const sm2_dsa_prikey =
    "3945208F7B2144B13F36E38AC6D39F95889393692860B51A42FB81EF4DF7C5B8";
static const char *const sm2_dsa_pubkey_x =
    "09F9DF311E5421A150DD7D161E4BC5C672179FAD1833FC076BB08FF356F35020";
static const char *const sm2_dsa_pubkey_y =
    "CCEA490CE26775A52DC6EA718CC1AA600AED05FBF35E084A6632F6072DA9AD13";

static const char sm2_dsa_ID[] = {
    0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38,
};

static const unsigned char sm2_dsa_z[] = {
    0xB2, 0xE1, 0x4C, 0x5C, 0x79, 0xC6, 0xDF, 0x5B, 0x85, 0xF4, 0xFE, 0x7E, 0xD8, 0xDB, 0x7A, 0x26,
    0x2B, 0x9D, 0xA7, 0xE0, 0x7C, 0xCB, 0x0E, 0xA9, 0xF4, 0x74, 0x7B, 0x8C, 0xCD, 0xA8, 0xA4, 0xF3};

static const unsigned char sm2_dsa_md[] = {
    0xF0, 0xB4, 0x3E, 0x94, 0xBA, 0x45, 0xAC, 0xCA, 0xAC, 0xE6, 0x92, 0xED, 0x53, 0x43, 0x82, 0xEB,
    0x17, 0xE6, 0xAB, 0x5A, 0x19, 0xCE, 0x7B, 0x31, 0xF4, 0x48, 0x6F, 0xDF, 0xC0, 0xD2, 0x86, 0x40};

static const unsigned char sm2_dsa_rand_k[] = {
    0x59, 0x27, 0x6E, 0x27, 0xD5, 0x06, 0x86, 0x1A, 0x16, 0x68, 0x0F, 0x3A, 0xD9, 0xC0, 0x2D, 0xCC,
    0xEF, 0x3C, 0xC1, 0xFA, 0x3C, 0xDB, 0xE4, 0xCE, 0x6D, 0x54, 0xB8, 0x0D, 0xEA, 0xC1, 0xBC, 0x21};

static const unsigned char sm2_dsa_sign[] = {
    0xF5, 0xA0, 0x3B, 0x06, 0x48, 0xD2, 0xC4, 0x63, 0x0E, 0xEA, 0xC5, 0x13, 0xE1, 0xBB, 0x81, 0xA1,
    0x59, 0x44, 0xDA, 0x38, 0x27, 0xD5, 0xB7, 0x41, 0x43, 0xAC, 0x7E, 0xAC, 0xEE, 0xE7, 0x20, 0xB3,

    0xB1, 0xB6, 0xAA, 0x29, 0xDF, 0x21, 0x2F, 0xD8, 0x76, 0x31, 0x82, 0xBC, 0x0D, 0x42, 0x1C, 0xA1,
    0xBB, 0x90, 0x38, 0xFD, 0x1F, 0x7F, 0x42, 0xD4, 0x84, 0x0B, 0x69, 0xC4, 0x85, 0xBB, 0xC1, 0xAA};

static int sm2_set_fix_rng(void *p_rng, unsigned char *buf, size_t size)
{
    memmove(buf, p_rng, size);
    return 0;
}

static int mbedtls_sm2_self_test_enc_dec(int verbose)
{
    int ret = 0;
    mbedtls_sm2_context ctx;
    unsigned char output[512];
    size_t olen;

    mbedtls_sm2_init(&ctx);
    if ((ret = mbedtls_ecp_group_load(&ctx.grp, MBEDTLS_ECP_DP_SM2P256R1)) != 0) {
        mbedtls_printf("load group failed\n");
        goto cleanup;
    }

    if (verbose != 0) mbedtls_printf("  SM2 key validation: ");

    if ((ret = mbedtls_mpi_read_string(&ctx.d, 16, sm2_enc_prikey)) != 0) {
        mbedtls_printf("read private key1 failed\n");
        goto cleanup;
    }

    if ((ret = mbedtls_ecp_point_read_string(&ctx.Q, 16, sm2_enc_pubkey_x, sm2_enc_pubkey_y))
        != 0) {
        mbedtls_printf("read public key1 failed\n");
        goto cleanup;
    }

    if (((ret = mbedtls_ecp_check_pubkey(&ctx.grp, &ctx.Q)) != 0)
        || (ret = mbedtls_ecp_check_privkey(&ctx.grp, &ctx.d) != 0)) {
        if (verbose != 0) { mbedtls_printf("failed\n"); }
        goto cleanup;
    }

    if (verbose) mbedtls_printf("passed\n  SM2 encryption: ");

    if ((ret = mbedtls_sm2_encrypt_raw(&ctx, MBEDTLS_MD_SM3, sm2_test_plaintext,
                                       sizeof(sm2_test_plaintext), output, &olen, sm2_set_fix_rng,
                                       (void *)sm2_enc_rand_k))
        != 0) {
        if (verbose != 0) { mbedtls_printf("failed with %d\n", ret); }
        goto cleanup;
    }

    if (memcmp(output, sm2_enc_ciphertext, sizeof(sm2_enc_ciphertext)) != 0) {
        if (verbose != 0) { mbedtls_printf("check failed\n"); }
        ret = 1;
        goto cleanup;
    }

    if (verbose != 0) { mbedtls_printf("passed\n  SM2 decryption: "); }

    if ((ret = mbedtls_sm2_decrypt_raw(&ctx, MBEDTLS_MD_SM3, sm2_enc_ciphertext,
                                       sizeof(sm2_enc_ciphertext), output, &olen))
        != 0) {
        if (verbose != 0) { mbedtls_printf("failed\n"); }
        goto cleanup;
    }
    if (memcmp(output, sm2_test_plaintext, sizeof(sm2_test_plaintext)) != 0) {
        if (verbose != 0) { mbedtls_printf("check failed\n"); }
        ret = 1;
        goto cleanup;
    }

    if (verbose != 0) { mbedtls_printf("passed\n"); }

    if (verbose != 0) { mbedtls_printf("\n"); }
cleanup:
    mbedtls_sm2_free(&ctx);
    return ret;
}

static int mbedtls_sm2_self_test_dsa(int verbose)
{
    int ret = 0;
    mbedtls_sm2_context ctx;
    unsigned char output[512];
    size_t olen;

    mbedtls_sm2_init(&ctx);
    if ((ret = mbedtls_ecp_group_load(&ctx.grp, MBEDTLS_ECP_DP_SM2P256R1)) != 0) {
        mbedtls_printf("load group failed\n");
        goto cleanup;
    }

    if (verbose != 0) mbedtls_printf("  SM2 DSA key validation: ");

    if ((ret = mbedtls_mpi_read_string(&ctx.d, 16, sm2_dsa_prikey)) != 0) {
        mbedtls_printf("read private dsa key failed\n");
        goto cleanup;
    }

    if ((ret = mbedtls_ecp_point_read_string(&ctx.Q, 16, sm2_dsa_pubkey_x, sm2_dsa_pubkey_y))
        != 0) {
        mbedtls_printf("read public dsa key failed\n");
        goto cleanup;
    }

    if (((ret = mbedtls_ecp_check_pubkey(&ctx.grp, &ctx.Q)) != 0)
        || (ret = mbedtls_ecp_check_privkey(&ctx.grp, &ctx.d) != 0)) {
        if (verbose != 0) { mbedtls_printf("failed\n"); }
        goto cleanup;
    }

    if (verbose != 0) { mbedtls_printf("passed\n  SM2 Get Z: "); }

    if ((ret = mbedtls_sm2_hash_z(&ctx, MBEDTLS_MD_SM3, sm2_dsa_ID, sizeof(sm2_dsa_ID), output))
        != 0) {
        if (verbose != 0) { mbedtls_printf("failed\n"); }
        goto cleanup;
    }
    if (memcmp(output, sm2_dsa_z, sizeof(sm2_dsa_z)) != 0) {
        if (verbose != 0) { mbedtls_printf("check failed\n"); }
        ret = 1;
        goto cleanup;
    }

    if (verbose != 0) { mbedtls_printf("passed\n  SM2 Get hash: "); }

    if ((ret = mbedtls_sm2_hash_e(MBEDTLS_MD_SM3, sm2_dsa_z, sm2_test_messagetext,
                                  sizeof(sm2_test_messagetext), output))
        != 0) {
        if (verbose != 0) { mbedtls_printf("failed\n"); }
        goto cleanup;
    }
    if (memcmp(output, sm2_dsa_md, sizeof(sm2_dsa_md)) != 0) {
        if (verbose != 0) { mbedtls_printf("check failed\n"); }
        ret = 1;
        goto cleanup;
    }

    if (verbose != 0) { mbedtls_printf("passed\n  SM2 sign: "); }

    if ((ret = mbedtls_sm2_sign_raw(&ctx, MBEDTLS_MD_SM3, sm2_dsa_md, output, &olen,
                                    sm2_set_fix_rng, (void *)sm2_dsa_rand_k))
        != 0) {
        if (verbose != 0) { mbedtls_printf("failed\n"); }
        goto cleanup;
    }
    if (memcmp(output, sm2_dsa_sign, sizeof(sm2_dsa_sign)) != 0) {
        if (verbose != 0) { mbedtls_printf("check failed\n"); }
        ret = 1;
        goto cleanup;
    }

    if (verbose != 0) { mbedtls_printf("passed\n  SM2 verify: "); }

    if ((ret = mbedtls_sm2_verify_raw(&ctx, MBEDTLS_MD_SM3, sm2_dsa_md, sizeof(sm2_dsa_md),
                                      sm2_dsa_sign))
        != 0) {
        if (verbose != 0) { mbedtls_printf("failed\n"); }
        goto cleanup;
    }

    if (verbose != 0) { mbedtls_printf("passed\n"); }

    if (verbose != 0) { mbedtls_printf("\n"); }
cleanup:
    mbedtls_sm2_free(&ctx);
    return ret;
}

int mbedtls_sm2_self_test(int verbose)
{
    int ret = 0;
    if ((ret = mbedtls_sm2_self_test_enc_dec(verbose)) != 0) { return ret; }

    if ((ret = mbedtls_sm2_self_test_dsa(verbose)) != 0) { return ret; }

    return ret;
}

#endif /* MBEDTLS_SELF_TEST */

#endif /* MBEDTLS_SM2_C */