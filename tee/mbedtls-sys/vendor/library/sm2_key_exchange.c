#include "common.h"


#include "mbedtls/sm2_key_exchange.h"
#include "mbedtls/platform_util.h"
#include "mbedtls/error.h"
#include "mbedtls/sm3.h"

#include <string.h>

/* Parameter validation macros based on platform_util.h */
#define SM2_VALIDATE_RET(cond)    \
    MBEDTLS_INTERNAL_VALIDATE_RET(cond, MBEDTLS_ERR_ECP_BAD_INPUT_DATA)
#define SM2_VALIDATE(cond)        \
    MBEDTLS_INTERNAL_VALIDATE(cond)

static mbedtls_ecp_group_id mbedtls_sm2_grp_id(
    const mbedtls_sm2_key_exchange_context *ctx)
{
    return ctx->grp.id;
}

int mbedtls_sm2_can_do(mbedtls_ecp_group_id gid)
{
    /* At this time, all groups support SM2. */
    (void) gid;
    return 1;
}

static int sm2_gen_public_restartable(mbedtls_ecp_group *grp,
                                       mbedtls_mpi *d, mbedtls_ecp_point *Q,
                                       int (*f_rng)(void *, unsigned char *, size_t),
                                       void *p_rng,
                                       mbedtls_ecp_restart_ctx *rs_ctx)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;

    int restarting = 0;
#if defined(MBEDTLS_ECP_RESTARTABLE)
    restarting = (rs_ctx != NULL && rs_ctx->rsm != NULL);
#endif
    /* If multiplication is in progress, we already generated a privkey */
    if (!restarting) {
        MBEDTLS_MPI_CHK(mbedtls_ecp_gen_privkey(grp, d, f_rng, p_rng));
    }

    MBEDTLS_MPI_CHK(mbedtls_ecp_mul_restartable(grp, Q, d, &grp->G,
                                                f_rng, p_rng, rs_ctx));

cleanup:
    return ret;
}

/*
 * Generate public key
 */
int mbedtls_sm2_gen_public(mbedtls_ecp_group *grp, mbedtls_mpi *d, mbedtls_ecp_point *Q,
                            int (*f_rng)(void *, unsigned char *, size_t),
                            void *p_rng)
{
    SM2_VALIDATE_RET(grp != NULL);
    SM2_VALIDATE_RET(d != NULL);
    SM2_VALIDATE_RET(Q != NULL);
    SM2_VALIDATE_RET(f_rng != NULL);
    return sm2_gen_public_restartable(grp, d, Q, f_rng, p_rng, NULL);
}

//依据SM2密钥交换标准，计算双方共享参数Z
int mbedtls_sm2_z(mbedtls_sm2_key_exchange_context *ctx, uint8_t endpoint){
    unsigned char buf[202];
    uint16_t bit_len = 64;
    buf[0] = (bit_len >> 8) & 0xFF;
    buf[1] = bit_len & 0xFF;
    buf[2] = (ctx->user_id >> 56) & 0xFF;
    buf[3] = (ctx->user_id >> 48) & 0xFF;
    buf[4] = (ctx->user_id >> 40) & 0xFF;
    buf[5] = (ctx->user_id >> 32) & 0xFF;
    buf[6] = (ctx->user_id >> 24) & 0xFF;
    buf[7] = (ctx->user_id >> 16) & 0xFF;
    buf[8] = (ctx->user_id >> 8) & 0xFF;
    buf[9] = ctx->user_id & 0xFF;
    mbedtls_mpi_write_binary(&ctx->grp.A,buf+10,32);
    mbedtls_mpi_write_binary(&ctx->grp.B,buf+42,32);
    mbedtls_mpi_write_binary(&ctx->grp.G.X,buf+74,32);
    mbedtls_mpi_write_binary(&ctx->grp.G.Y,buf+106,32);
    mbedtls_mpi_write_binary(&ctx->R.X,buf+138,32);
    mbedtls_mpi_write_binary(&ctx->R.Y,buf+170,32);


    unsigned char buf1[202];
    buf1[0] = (bit_len >> 8) & 0xFF;
    buf1[1] = bit_len & 0xFF;
    buf1[2] = (ctx->user_id_p >> 56) & 0xFF;
    buf1[3] = (ctx->user_id_p >> 48) & 0xFF;
    buf1[4] = (ctx->user_id_p >> 40) & 0xFF;
    buf1[5] = (ctx->user_id_p >> 32) & 0xFF;
    buf1[6] = (ctx->user_id_p >> 24) & 0xFF;
    buf1[7] = (ctx->user_id_p >> 16) & 0xFF;
    buf1[8] = (ctx->user_id_p >> 8) & 0xFF;
    buf1[9] = ctx->user_id_p & 0xFF;
    mbedtls_mpi_write_binary(&ctx->grp.A,buf1+10,32);
    mbedtls_mpi_write_binary(&ctx->grp.B,buf1+42,32);
    mbedtls_mpi_write_binary(&ctx->grp.G.X,buf1+74,32);
    mbedtls_mpi_write_binary(&ctx->grp.G.Y,buf1+106,32);
    mbedtls_mpi_write_binary(&ctx->Rp.X,buf1+138,32);
    mbedtls_mpi_write_binary(&ctx->Rp.Y,buf1+170,32);
    //Client
    if(endpoint == 0){
        mbedtls_sm3_ret(buf,202,ctx->Z);
        mbedtls_sm3_ret(buf1,202,ctx->Zp);
    }
    //Server
    else{
        mbedtls_sm3_ret(buf,202,ctx->Zp);
        mbedtls_sm3_ret(buf1,202,ctx->Z);
    }
    return 0;
}

//SM2密钥交换的密钥派生,派生SM4密钥
static int sm2_hkdf(mbedtls_sm2_key_exchange_context *ctx, mbedtls_ecp_point V, unsigned char *okm){
    unsigned char info[128];
    mbedtls_mpi_write_binary(&V.X,info,32);
    mbedtls_mpi_write_binary(&V.Y,info + 32,32);
    memcpy(info + 64, ctx->Z, 32);
    memcpy(info + 96, ctx->Zp, 32);

    unsigned char counter[4];
    unsigned char digest[32];
    uint32_t ct = 1;
    mbedtls_sm3_context sm3_ctx;
    
    counter[0] = (ct >> 24) & 0xFF;
    counter[1] = (ct >> 16) & 0xFF;
    counter[2] = (ct >> 8) & 0xFF;
    counter[3] = ct & 0xFF;

    mbedtls_sm3_init(&sm3_ctx);
    mbedtls_sm3_update_ret(&sm3_ctx, info, 128);
    mbedtls_sm3_update_ret(&sm3_ctx, counter, 4);
    mbedtls_sm3_finish_ret(&sm3_ctx, digest);
    memcpy(okm, digest, 16);
    return 0;
}

static int mbedtls_mpi_and_mpi(mbedtls_mpi *X, const mbedtls_mpi *A, const mbedtls_mpi *B)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
    size_t i, max_limbs, min_limbs;

    if (A->n >= B->n) {
        max_limbs = A->n;
        min_limbs = B->n;
    } else {
        max_limbs = B->n;
        min_limbs = A->n;
    }

    MBEDTLS_MPI_CHK(mbedtls_mpi_grow(X, max_limbs));

    for (i = 0; i < min_limbs; i++) {
        X->p[i] = A->p[i] & B->p[i];
    }
    for (i = min_limbs; i < max_limbs; i++) {
        X->p[i] = 0;
    }
    X->s = 1;

    MBEDTLS_MPI_CHK(mbedtls_mpi_shrink(X, min_limbs));

cleanup:
    return ret;
}

/*
 * Compute shared secret (SEC1 3.3.1)
 */
static int sm2_compute_shared_restartable(mbedtls_sm2_key_exchange_context *ctx,
                                           int (*f_rng)(void *, unsigned char *, size_t),
                                           void *p_rng,
                                           mbedtls_ecp_restart_ctx *rs_ctx)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
    int w = 127;
    mbedtls_mpi T1, T2, X;
    mbedtls_mpi_init(&T1);
    mbedtls_mpi_init(&T2);
    mbedtls_mpi_init(&X);
    mbedtls_mpi_lset(&T1, 1<<w);
    mbedtls_mpi_lset(&T2, (1<<w) - 1);
    mbedtls_mpi_and_mpi(&X, &T2, &ctx->Q.X);
    mbedtls_mpi_add_mpi(&X, &T1, &X);

    mbedtls_mpi T3, T;
    mbedtls_mpi_init(&T3);
    mbedtls_mpi_init(&T);
    mbedtls_mpi_mul_mpi(&T3, &X, &ctx->d);
    mbedtls_mpi_add_mpi(&T, &T3, &ctx->r);
    mbedtls_mpi_mod_mpi(&T, &T, &ctx->grp.N);

    mbedtls_mpi Xp;
    mbedtls_mpi_init(&Xp);
    mbedtls_mpi_and_mpi(&Xp, &T2, &ctx->Qp.X);
    mbedtls_mpi_add_mpi(&Xp, &T1, &Xp);
    mbedtls_mpi_mod_mpi(&Xp, &Xp, &ctx->grp.N);

    mbedtls_ecp_point V;
    mbedtls_mpi num1;
    mbedtls_ecp_point_init(&V);
    mbedtls_mpi_init(&num1);
    mbedtls_mpi_lset(&num1, 1);

    MBEDTLS_MPI_CHK(mbedtls_ecp_muladd_restartable(&ctx->grp, &V, &num1, &ctx->Rp, &Xp, &ctx->Qp, rs_ctx));
    MBEDTLS_MPI_CHK(mbedtls_ecp_mul_restartable(&ctx->grp, &V, &T, &V, f_rng, p_rng, rs_ctx));

    if (mbedtls_ecp_is_zero(&V)) {
        ret = MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
        goto cleanup;
    }

    unsigned char key[16];
    sm2_hkdf(ctx, V, key);
    mbedtls_mpi_read_binary(&ctx->z, key, 16);

cleanup:
    mbedtls_ecp_point_free(&V);
    mbedtls_mpi_free(&T);
    mbedtls_mpi_free(&T1);
    mbedtls_mpi_free(&T2);
    mbedtls_mpi_free(&T3);
    mbedtls_mpi_free(&X);
    mbedtls_mpi_free(&Xp);
    mbedtls_mpi_free(&num1);

    return ret;
}

/*
 * Compute shared secret (SEC1 3.3.1)
 */
int mbedtls_sm2_compute_shared(mbedtls_sm2_key_exchange_context *ctx,
                                int (*f_rng)(void *, unsigned char *, size_t),
                                void *p_rng)
{
    SM2_VALIDATE_RET(grp != NULL);
    SM2_VALIDATE_RET(Q != NULL);
    SM2_VALIDATE_RET(d != NULL);
    SM2_VALIDATE_RET(z != NULL);
    return sm2_compute_shared_restartable(ctx,f_rng, p_rng, NULL);
}


static void sm2_init_internal(mbedtls_sm2_key_exchange_context *ctx)
{
    mbedtls_ecp_group_init(&ctx->grp);
    mbedtls_mpi_init(&ctx->d);
    mbedtls_ecp_point_init(&ctx->Q);
    mbedtls_ecp_point_init(&ctx->Qp);
    mbedtls_mpi_init(&ctx->r);
    mbedtls_ecp_point_init(&ctx->R);
    mbedtls_ecp_point_init(&ctx->Rp);
    mbedtls_mpi_init(&ctx->z);

#if defined(MBEDTLS_ECP_RESTARTABLE)
    mbedtls_ecp_restart_init(&ctx->rs);
#endif
}

/*
 * Initialize context
 */
void mbedtls_sm2_key_exchange_init(mbedtls_sm2_key_exchange_context *ctx)
{
    SM2_VALIDATE(ctx != NULL);

    sm2_init_internal(ctx);
    mbedtls_ecp_point_init(&ctx->Vi);
    mbedtls_ecp_point_init(&ctx->Vf);
    mbedtls_mpi_init(&ctx->_d);
    ctx->point_format = MBEDTLS_ECP_PF_UNCOMPRESSED;
#if defined(MBEDTLS_ECP_RESTARTABLE)
    ctx->restart_enabled = 0;
#endif
}

static int sm2_setup_internal(mbedtls_sm2_key_exchange_context *ctx,
                               mbedtls_ecp_group_id grp_id)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;

    ret = mbedtls_ecp_group_load(&ctx->grp, grp_id);
    if (ret != 0) {
        return MBEDTLS_ERR_ECP_FEATURE_UNAVAILABLE;
    }

    return 0;
}

/*
 * Setup context
 */
int mbedtls_sm2_setup(mbedtls_sm2_key_exchange_context *ctx, mbedtls_ecp_group_id grp_id)
{
    SM2_VALIDATE_RET(ctx != NULL);
    return sm2_setup_internal(ctx, grp_id);
}

static void sm2_free_internal(mbedtls_sm2_key_exchange_context *ctx)
{
    mbedtls_ecp_group_free(&ctx->grp);
    mbedtls_mpi_free(&ctx->d);
    mbedtls_ecp_point_free(&ctx->Q);
    mbedtls_ecp_point_free(&ctx->Qp);
    mbedtls_mpi_free(&ctx->r);
    mbedtls_ecp_point_free(&ctx->R);
    mbedtls_ecp_point_free(&ctx->Rp);
    mbedtls_mpi_free(&ctx->z);

#if defined(MBEDTLS_ECP_RESTARTABLE)
    mbedtls_sm2_restart_free(&ctx->rs);
#endif
}

#if defined(MBEDTLS_ECP_RESTARTABLE)
/*
 * Enable restartable operations for context
 */
void mbedtls_sm2_enable_restart(mbedtls_sm2_key_exchange_context *ctx)
{
    SM2_VALIDATE(ctx != NULL);

    ctx->restart_enabled = 1;
}
#endif

/*
 * Free context
 */
void mbedtls_sm2_key_exchange_free(mbedtls_sm2_key_exchange_context *ctx)
{
    if (ctx == NULL) {
        return;
    }

    mbedtls_ecp_point_free(&ctx->Vi);
    mbedtls_ecp_point_free(&ctx->Vf);
    mbedtls_mpi_free(&ctx->_d);
    sm2_free_internal(ctx);
}

static int sm2_make_params_internal(mbedtls_sm2_key_exchange_context *ctx,
                                     size_t *olen, int point_format,
                                     unsigned char *buf, size_t blen,
                                     int (*f_rng)(void *,
                                                  unsigned char *,
                                                  size_t),
                                     void *p_rng,
                                     int restart_enabled)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
    size_t grp_len, pt_len;
#if defined(MBEDTLS_ECP_RESTARTABLE)
    mbedtls_ecp_restart_ctx *rs_ctx = NULL;
#endif

    if (ctx->grp.pbits == 0) {
        return MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
    }

#if defined(MBEDTLS_ECP_RESTARTABLE)
    if (restart_enabled) {
        rs_ctx = &ctx->rs;
    }
#else
    (void) restart_enabled;
#endif


#if defined(MBEDTLS_ECP_RESTARTABLE)
    if ((ret = sm2_gen_public_restartable(&ctx->grp, &ctx->d, &ctx->Q,
                                           f_rng, p_rng, rs_ctx)) != 0) {
        return ret;
    }
#else
    if ((ret = mbedtls_sm2_gen_public(&ctx->grp, &ctx->d, &ctx->Q,
                                       f_rng, p_rng)) != 0) {
        return ret;
    }
#endif /* MBEDTLS_ECP_RESTARTABLE */

    if ((ret = mbedtls_ecp_tls_write_group(&ctx->grp, &grp_len, buf,
                                           blen)) != 0) {
        return ret;
    }

    buf += grp_len;
    blen -= grp_len;

    if ((ret = mbedtls_ecp_tls_write_point(&ctx->grp, &ctx->Q, point_format,
                                           &pt_len, buf, blen)) != 0) {
        return ret;
    }

    *olen = grp_len + pt_len;
    return 0;
}

/*
 * Setup and write the ServerKeyExchange parameters (RFC 4492)
 *      struct {
 *          ECParameters    curve_params;
 *          ECPoint         public;
 *      } ServerSM2Params;
 */
int mbedtls_sm2_make_params(mbedtls_sm2_key_exchange_context *ctx, size_t *olen,
                             unsigned char *buf, size_t blen,
                             int (*f_rng)(void *, unsigned char *, size_t),
                             void *p_rng)
{
    int restart_enabled = 0;
    SM2_VALIDATE_RET(ctx != NULL);
    SM2_VALIDATE_RET(olen != NULL);
    SM2_VALIDATE_RET(buf != NULL);
    SM2_VALIDATE_RET(f_rng != NULL);

#if defined(MBEDTLS_ECP_RESTARTABLE)
    restart_enabled = ctx->restart_enabled;
#else
    (void) restart_enabled;
#endif

    return sm2_make_params_internal(ctx, olen, ctx->point_format, buf, blen,
                                     f_rng, p_rng, restart_enabled);
}

static int sm2_read_params_internal(mbedtls_sm2_key_exchange_context *ctx,
                                     const unsigned char **buf,
                                     const unsigned char *end)
{
    return mbedtls_ecp_tls_read_point(&ctx->grp, &ctx->Qp, buf,
                                      end - *buf);
}

/*
 * Read the ServerKeyExchange parameters (RFC 4492)
 *      struct {
 *          ECParameters    curve_params;
 *          ECPoint         public;
 *      } ServerSM2Params;
 */
int mbedtls_sm2_read_params(mbedtls_sm2_key_exchange_context *ctx,
                             const unsigned char **buf,
                             const unsigned char *end)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
    mbedtls_ecp_group_id grp_id;
    SM2_VALIDATE_RET(ctx != NULL);
    SM2_VALIDATE_RET(buf != NULL);
    SM2_VALIDATE_RET(*buf != NULL);
    SM2_VALIDATE_RET(end != NULL);

    if ((ret = mbedtls_ecp_tls_read_group_id(&grp_id, buf, end - *buf))
        != 0) {
        return ret;
    }

    if ((ret = mbedtls_sm2_setup(ctx, grp_id)) != 0) {
        return ret;
    }

    return sm2_read_params_internal(ctx, buf, end);
}

static int sm2_get_params_internal(mbedtls_sm2_key_exchange_context *ctx,
                                    const mbedtls_ecp_keypair *key,
                                    mbedtls_sm2_side side)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;

    /* If it's not our key, just import the public part as Qp */
    if (side == MBEDTLS_SM2_THEIRS) {
        return mbedtls_ecp_copy(&ctx->Qp, &key->Q);
    }

    /* Our key: import public (as Q) and private parts */
    if (side != MBEDTLS_SM2_OURS) {
        return MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
    }

    if ((ret = mbedtls_ecp_copy(&ctx->Q, &key->Q)) != 0 ||
        (ret = mbedtls_mpi_copy(&ctx->d, &key->d)) != 0) {
        return ret;
    }

    return 0;
}

/*
 * Get parameters from a keypair
 */
int mbedtls_sm2_get_params(mbedtls_sm2_key_exchange_context *ctx,
                            const mbedtls_ecp_keypair *key,
                            mbedtls_sm2_side side)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
    SM2_VALIDATE_RET(ctx != NULL);
    SM2_VALIDATE_RET(key != NULL);
    SM2_VALIDATE_RET(side == MBEDTLS_SM2_OURS ||
                      side == MBEDTLS_SM2_THEIRS);

    if (mbedtls_sm2_grp_id(ctx) == MBEDTLS_ECP_DP_NONE) {
        /* This is the first call to get_params(). Set up the context
         * for use with the group. */
        if ((ret = mbedtls_sm2_setup(ctx, key->grp.id)) != 0) {
            return ret;
        }
    } else {
        /* This is not the first call to get_params(). Check that the
         * current key's group is the same as the context's, which was set
         * from the first key's group. */
        if (mbedtls_sm2_grp_id(ctx) != key->grp.id) {
            return MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
        }
    }

    return sm2_get_params_internal(ctx, key, side);
}

static int sm2_make_public_internal(mbedtls_sm2_key_exchange_context *ctx,
                                     size_t *olen, int point_format,
                                     unsigned char *buf, size_t blen,
                                     int (*f_rng)(void *,
                                                  unsigned char *,
                                                  size_t),
                                     void *p_rng,
                                     int restart_enabled)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
#if defined(MBEDTLS_ECP_RESTARTABLE)
    mbedtls_ecp_restart_ctx *rs_ctx = NULL;
#endif

    if (ctx->grp.pbits == 0) {
        return MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
    }

#if defined(MBEDTLS_ECP_RESTARTABLE)
    if (restart_enabled) {
        rs_ctx = &ctx->rs;
    }
#else
    (void) restart_enabled;
#endif

#if defined(MBEDTLS_ECP_RESTARTABLE)
    if ((ret = sm2_gen_public_restartable(&ctx->grp, &ctx->d, &ctx->Q,
                                           f_rng, p_rng, rs_ctx)) != 0) {
        return ret;
    }
#else
    if ((ret = mbedtls_sm2_gen_public(&ctx->grp, &ctx->d, &ctx->Q,
                                       f_rng, p_rng)) != 0) {
        return ret;
    }
#endif /* MBEDTLS_ECP_RESTARTABLE */

    return mbedtls_ecp_tls_write_point(&ctx->grp, &ctx->Q, point_format, olen,
                                       buf, blen);
}

/*
 * Setup and export the client public value
 */
int mbedtls_sm2_make_public(mbedtls_sm2_key_exchange_context *ctx, size_t *olen,
                             unsigned char *buf, size_t blen,
                             int (*f_rng)(void *, unsigned char *, size_t),
                             void *p_rng)
{
    int restart_enabled = 0;
    SM2_VALIDATE_RET(ctx != NULL);
    SM2_VALIDATE_RET(olen != NULL);
    SM2_VALIDATE_RET(buf != NULL);
    SM2_VALIDATE_RET(f_rng != NULL);

#if defined(MBEDTLS_ECP_RESTARTABLE)
    restart_enabled = ctx->restart_enabled;
#endif

    return sm2_make_public_internal(ctx, olen, ctx->point_format, buf, blen,
                                     f_rng, p_rng, restart_enabled);
}

static int sm2_read_public_internal(mbedtls_sm2_key_exchange_context *ctx,
                                     const unsigned char *buf, size_t blen)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
    const unsigned char *p = buf;

    if ((ret = mbedtls_ecp_tls_read_point(&ctx->grp, &ctx->Qp, &p,
                                          blen)) != 0) {
        return ret;
    }

    if ((size_t) (p - buf) != blen) {
        return MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
    }

    return 0;
}

/*
 * Parse and import the client's public value
 */
int mbedtls_sm2_read_public(mbedtls_sm2_key_exchange_context *ctx,
                             const unsigned char *buf, size_t blen)
{
    SM2_VALIDATE_RET(ctx != NULL);
    SM2_VALIDATE_RET(buf != NULL);

    return sm2_read_public_internal(ctx, buf, blen);
}

static int sm2_calc_secret_internal(mbedtls_sm2_key_exchange_context *ctx,
                                     size_t *olen, unsigned char *buf,
                                     size_t blen,
                                     int (*f_rng)(void *,
                                                  unsigned char *,
                                                  size_t),
                                     void *p_rng,
                                     int restart_enabled)
{
    int ret = MBEDTLS_ERR_ERROR_CORRUPTION_DETECTED;
#if defined(MBEDTLS_ECP_RESTARTABLE)
    mbedtls_ecp_restart_ctx *rs_ctx = NULL;
#endif

    if (ctx == NULL || ctx->grp.pbits == 0) {
        return MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
    }

#if defined(MBEDTLS_ECP_RESTARTABLE)
    if (restart_enabled) {
        rs_ctx = &ctx->rs;
    }
#else
    (void) restart_enabled;
#endif

#if defined(MBEDTLS_ECP_RESTARTABLE)
    if ((ret = sm2_compute_shared_restartable(&ctx->grp, &ctx->z, &ctx->Qp,
                                               &ctx->d, f_rng, p_rng,
                                               rs_ctx)) != 0) {
        return ret;
    }
#else
    if ((ret = mbedtls_sm2_compute_shared(ctx, f_rng, p_rng)) != 0) {
        return ret;
    }
#endif /* MBEDTLS_ECP_RESTARTABLE */

    if (mbedtls_mpi_size(&ctx->z) > blen) {
        return MBEDTLS_ERR_ECP_BAD_INPUT_DATA;
    }

    *olen = ctx->grp.pbits / 8 + ((ctx->grp.pbits % 8) != 0);

    if (mbedtls_ecp_get_type(&ctx->grp) == MBEDTLS_ECP_TYPE_MONTGOMERY) {
        return mbedtls_mpi_write_binary_le(&ctx->z, buf, *olen);
    }

    return mbedtls_mpi_write_binary(&ctx->z, buf, *olen);
}

/*
 * Derive and export the shared secret
 */
int mbedtls_sm2_calc_secret(mbedtls_sm2_key_exchange_context *ctx, size_t *olen,
                             unsigned char *buf, size_t blen,
                             int (*f_rng)(void *, unsigned char *, size_t),
                             void *p_rng)
{
    int restart_enabled = 0;
    SM2_VALIDATE_RET(ctx != NULL);
    SM2_VALIDATE_RET(olen != NULL);
    SM2_VALIDATE_RET(buf != NULL);

#if defined(MBEDTLS_ECP_RESTARTABLE)
    restart_enabled = ctx->restart_enabled;
#endif

    return sm2_calc_secret_internal(ctx, olen, buf, blen, f_rng, p_rng,
                                     restart_enabled);
}

int mbedtls_sm2_set_key(mbedtls_sm2_key_exchange_context *ctx, mbedtls_sm2_context *sm2_ctx){
    if(ctx == NULL || sm2_ctx == NULL){
        return MBEDTLS_ERR_SM2_BAD_INPUT_DATA;
    }
    ctx->r = sm2_ctx->d;
    ctx->R = sm2_ctx->Q;
    return 0;
}

int mbedtls_sm2_set_peerkey(mbedtls_sm2_key_exchange_context *ctx, mbedtls_sm2_context *sm2_ctx){
    if(ctx == NULL || sm2_ctx == NULL){
        return MBEDTLS_ERR_SM2_BAD_INPUT_DATA;
    }
    ctx->Rp = sm2_ctx->Q;
    return 0;
}