#ifndef MBEDTLS_SM2_KEY_EXCHANGE_H
#define MBEDTLS_SM2_KEY_EXCHANGE_H

#if !defined(MBEDTLS_CONFIG_FILE)
#include "mbedtls/config.h"
#else
#include MBEDTLS_CONFIG_FILE
#endif

#include "mbedtls/ecp.h"
#include "mbedtls/sm2.h"

#if defined(MBEDTLS_ECDH_VARIANT_EVEREST_ENABLED)
#undef MBEDTLS_ECDH_LEGACY_CONTEXT
#include "everest/everest.h"
#endif

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Defines the source of the imported SM2 key.
 */
typedef enum {
    MBEDTLS_SM2_OURS,   /**< Our key. */
    MBEDTLS_SM2_THEIRS, /**< The key of the peer. */
} mbedtls_sm2_side;

typedef struct mbedtls_sm2_key_exchange_context {
    mbedtls_ecp_group grp;   /*!< The elliptic curve used. */
    mbedtls_mpi d;           /*!< 临时私钥d */
    mbedtls_ecp_point Q;     /*!< 临时公钥Q */
    mbedtls_ecp_point Qp;    /*!< 对方的临时公钥Qp */
    mbedtls_mpi z;           /*!< The shared secret. */
    int point_format;        /*!< The format of point export in TLS messages. */
    mbedtls_ecp_point Vi;    /*!< The blinding value. */
    mbedtls_ecp_point Vf;    /*!< The unblinding value. */
    mbedtls_mpi _d;          /*!< The previous \p d. */
    unsigned char Z[32];     /*!< SM2密钥交换需要的发起方参数Z*/
    unsigned char Zp[32];    /*!< SM2密钥交换需要的接收方参数Zp*/
    mbedtls_mpi r;           /*!< 长期私钥r */
    mbedtls_ecp_point R;     /*!< 长期公钥R */
    mbedtls_ecp_point Rp;    /*!< 对方的长期公钥Rp */
    uint64_t user_id;        /*!< 用户的id，用于计算Z*/
    uint64_t user_id_p;      /*!< 用户的id，用于计算Zp*/
#if defined(MBEDTLS_ECP_RESTARTABLE)
    int restart_enabled;        /*!< The flag for restartable mode. */
    mbedtls_ecp_restart_ctx rs; /*!< The restart context for EC computations. */
#endif /* MBEDTLS_ECP_RESTARTABLE */
}
mbedtls_sm2_key_exchange_context;

/**
 * \brief          Check whether a given group can be used for SM2.
 *
 * \param gid      The ECP group ID to check.
 *
 * \return         \c 1 if the group can be used, \c 0 otherwise
 */
int mbedtls_sm2_can_do(mbedtls_ecp_group_id gid);

/**
 * \brief           This function generates an ECDH keypair on an elliptic
 *                  curve.
 *
 *                  This function performs the first of two core computations
 *                  implemented during the ECDH key exchange. The second core
 *                  computation is performed by mbedtls_ecdh_compute_shared().
 *
 * \see             ecp.h
 *
 * \param grp       The ECP group to use. This must be initialized and have
 *                  domain parameters loaded, for example through
 *                  mbedtls_ecp_load() or mbedtls_ecp_tls_read_group().
 * \param d         The destination MPI (private key).
 *                  This must be initialized.
 * \param Q         The destination point (public key).
 *                  This must be initialized.
 * \param f_rng     The RNG function to use. This must not be \c NULL.
 * \param p_rng     The RNG context to be passed to \p f_rng. This may be
 *                  \c NULL in case \p f_rng doesn't need a context argument.
 *
 * \return          \c 0 on success.
 * \return          Another \c MBEDTLS_ERR_ECP_XXX or
 *                  \c MBEDTLS_MPI_XXX error code on failure.
 */
int mbedtls_sm2_gen_public(mbedtls_ecp_group *grp, mbedtls_mpi *d, mbedtls_ecp_point *Q,
                            int (*f_rng)(void *, unsigned char *, size_t),
                            void *p_rng);


/**
 * \brief           This function computes the shared secret.
 *
 *                  This function performs the second of two core computations
 *                  implemented during the ECDH key exchange. The first core
 *                  computation is performed by mbedtls_ecdh_gen_public().
 *
 * \see             ecp.h
 *
 * \note            If \p f_rng is not NULL, it is used to implement
 *                  countermeasures against side-channel attacks.
 *                  For more information, see mbedtls_ecp_mul().
 *
 * \param grp       The ECP group to use. This must be initialized and have
 *                  domain parameters loaded, for example through
 *                  mbedtls_ecp_load() or mbedtls_ecp_tls_read_group().
 * \param z         The destination MPI (shared secret).
 *                  This must be initialized.
 * \param Q         The public key from another party.
 *                  This must be initialized.
 * \param d         Our secret exponent (private key).
 *                  This must be initialized.
 * \param f_rng     The RNG function. This may be \c NULL if randomization
 *                  of intermediate results during the ECP computations is
 *                  not needed (discouraged). See the documentation of
 *                  mbedtls_ecp_mul() for more.
 * \param p_rng     The RNG context to be passed to \p f_rng. This may be
 *                  \c NULL if \p f_rng is \c NULL or doesn't need a
 *                  context argument.
 *
 * \return          \c 0 on success.
 * \return          Another \c MBEDTLS_ERR_ECP_XXX or
 *                  \c MBEDTLS_MPI_XXX error code on failure.
 */
int mbedtls_sm2_compute_shared(mbedtls_sm2_key_exchange_context *ctx,
                                int (*f_rng)(void *, unsigned char *, size_t),
                                void *p_rng);

int mbedtls_sm2_z(mbedtls_sm2_key_exchange_context *ctx, uint8_t endpoint);
/**
 * \brief           This function initializes an ECDH context.
 *
 * \param ctx       The ECDH context to initialize. This must not be \c NULL.
 */
void mbedtls_sm2_key_exchange_init(mbedtls_sm2_key_exchange_context *ctx);

/**
 * \brief           This function sets up the ECDH context with the information
 *                  given.
 *
 *                  This function should be called after mbedtls_ecdh_init() but
 *                  before mbedtls_ecdh_make_params(). There is no need to call
 *                  this function before mbedtls_ecdh_read_params().
 *
 *                  This is the first function used by a TLS server for ECDHE
 *                  ciphersuites.
 *
 * \param ctx       The ECDH context to set up. This must be initialized.
 * \param grp_id    The group id of the group to set up the context for.
 *
 * \return          \c 0 on success.
 */
int mbedtls_sm2_setup(mbedtls_sm2_key_exchange_context *ctx,
                       mbedtls_ecp_group_id grp_id);

/**
 * \brief           This function frees a context.
 *
 * \param ctx       The context to free. This may be \c NULL, in which
 *                  case this function does nothing. If it is not \c NULL,
 *                  it must point to an initialized ECDH context.
 */
void mbedtls_sm2_key_exchange_free(mbedtls_sm2_key_exchange_context *ctx);

/**
 * \brief           This function generates an EC key pair and exports its
 *                  in the format used in a TLS ServerKeyExchange handshake
 *                  message.
 *
 *                  This is the second function used by a TLS server for ECDHE
 *                  ciphersuites. (It is called after mbedtls_ecdh_setup().)
 *
 * \see             ecp.h
 *
 * \param ctx       The ECDH context to use. This must be initialized
 *                  and bound to a group, for example via mbedtls_ecdh_setup().
 * \param olen      The address at which to store the number of Bytes written.
 * \param buf       The destination buffer. This must be a writable buffer of
 *                  length \p blen Bytes.
 * \param blen      The length of the destination buffer \p buf in Bytes.
 * \param f_rng     The RNG function to use. This must not be \c NULL.
 * \param p_rng     The RNG context to be passed to \p f_rng. This may be
 *                  \c NULL in case \p f_rng doesn't need a context argument.
 *
 * \return          \c 0 on success.
 * \return          #MBEDTLS_ERR_ECP_IN_PROGRESS if maximum number of
 *                  operations was reached: see \c mbedtls_ecp_set_max_ops().
 * \return          Another \c MBEDTLS_ERR_ECP_XXX error code on failure.
 */
int mbedtls_sm2_make_params(mbedtls_sm2_key_exchange_context *ctx, size_t *olen,
                             unsigned char *buf, size_t blen,
                             int (*f_rng)(void *, unsigned char *, size_t),
                             void *p_rng);

/**
 * \brief           This function parses the ECDHE parameters in a
 *                  TLS ServerKeyExchange handshake message.
 *
 * \note            In a TLS handshake, this is the how the client
 *                  sets up its ECDHE context from the server's public
 *                  ECDHE key material.
 *
 * \see             ecp.h
 *
 * \param ctx       The ECDHE context to use. This must be initialized.
 * \param buf       On input, \c *buf must be the start of the input buffer.
 *                  On output, \c *buf is updated to point to the end of the
 *                  data that has been read. On success, this is the first byte
 *                  past the end of the ServerKeyExchange parameters.
 *                  On error, this is the point at which an error has been
 *                  detected, which is usually not useful except to debug
 *                  failures.
 * \param end       The end of the input buffer.
 *
 * \return          \c 0 on success.
 * \return          An \c MBEDTLS_ERR_ECP_XXX error code on failure.
 *
 */
int mbedtls_sm2_read_params(mbedtls_sm2_key_exchange_context *ctx,
                             const unsigned char **buf,
                             const unsigned char *end);

/**
 * \brief           This function sets up an ECDH context from an EC key.
 *
 *                  It is used by clients and servers in place of the
 *                  ServerKeyExchange for static ECDH, and imports ECDH
 *                  parameters from the EC key information of a certificate.
 *
 * \see             ecp.h
 *
 * \param ctx       The ECDH context to set up. This must be initialized.
 * \param key       The EC key to use. This must be initialized.
 * \param side      Defines the source of the key. Possible values are:
 *                  - #MBEDTLS_ECDH_OURS: The key is ours.
 *                  - #MBEDTLS_ECDH_THEIRS: The key is that of the peer.
 *
 * \return          \c 0 on success.
 * \return          Another \c MBEDTLS_ERR_ECP_XXX error code on failure.
 *
 */
int mbedtls_sm2_get_params(mbedtls_sm2_key_exchange_context *ctx,
                            const mbedtls_ecp_keypair *key,
                            mbedtls_sm2_side side);

/**
 * \brief           This function generates a public key and exports it
 *                  as a TLS ClientKeyExchange payload.
 *
 *                  This is the second function used by a TLS client for ECDH(E)
 *                  ciphersuites.
 *
 * \see             ecp.h
 *
 * \param ctx       The ECDH context to use. This must be initialized
 *                  and bound to a group, the latter usually by
 *                  mbedtls_ecdh_read_params().
 * \param olen      The address at which to store the number of Bytes written.
 *                  This must not be \c NULL.
 * \param buf       The destination buffer. This must be a writable buffer
 *                  of length \p blen Bytes.
 * \param blen      The size of the destination buffer \p buf in Bytes.
 * \param f_rng     The RNG function to use. This must not be \c NULL.
 * \param p_rng     The RNG context to be passed to \p f_rng. This may be
 *                  \c NULL in case \p f_rng doesn't need a context argument.
 *
 * \return          \c 0 on success.
 * \return          #MBEDTLS_ERR_ECP_IN_PROGRESS if maximum number of
 *                  operations was reached: see \c mbedtls_ecp_set_max_ops().
 * \return          Another \c MBEDTLS_ERR_ECP_XXX error code on failure.
 */
int mbedtls_sm2_make_public(mbedtls_sm2_key_exchange_context *ctx, size_t *olen,
                             unsigned char *buf, size_t blen,
                             int (*f_rng)(void *, unsigned char *, size_t),
                             void *p_rng);

/**
 * \brief       This function parses and processes the ECDHE payload of a
 *              TLS ClientKeyExchange message.
 *
 *              This is the third function used by a TLS server for ECDH(E)
 *              ciphersuites. (It is called after mbedtls_ecdh_setup() and
 *              mbedtls_ecdh_make_params().)
 *
 * \see         ecp.h
 *
 * \param ctx   The ECDH context to use. This must be initialized
 *              and bound to a group, for example via mbedtls_ecdh_setup().
 * \param buf   The pointer to the ClientKeyExchange payload. This must
 *              be a readable buffer of length \p blen Bytes.
 * \param blen  The length of the input buffer \p buf in Bytes.
 *
 * \return      \c 0 on success.
 * \return      An \c MBEDTLS_ERR_ECP_XXX error code on failure.
 */
int mbedtls_sm2_read_public(mbedtls_sm2_key_exchange_context *ctx,
                             const unsigned char *buf, size_t blen);

/**
 * \brief           This function derives and exports the shared secret.
 *
 *                  This is the last function used by both TLS client
 *                  and servers.
 *
 * \note            If \p f_rng is not NULL, it is used to implement
 *                  countermeasures against side-channel attacks.
 *                  For more information, see mbedtls_ecp_mul().
 *
 * \see             ecp.h

 * \param ctx       The ECDH context to use. This must be initialized
 *                  and have its own private key generated and the peer's
 *                  public key imported.
 * \param olen      The address at which to store the total number of
 *                  Bytes written on success. This must not be \c NULL.
 * \param buf       The buffer to write the generated shared key to. This
 *                  must be a writable buffer of size \p blen Bytes.
 * \param blen      The length of the destination buffer \p buf in Bytes.
 * \param f_rng     The RNG function, for blinding purposes. This may
 *                  b \c NULL if blinding isn't needed.
 * \param p_rng     The RNG context. This may be \c NULL if \p f_rng
 *                  doesn't need a context argument.
 *
 * \return          \c 0 on success.
 * \return          #MBEDTLS_ERR_ECP_IN_PROGRESS if maximum number of
 *                  operations was reached: see \c mbedtls_ecp_set_max_ops().
 * \return          Another \c MBEDTLS_ERR_ECP_XXX error code on failure.
 */
int mbedtls_sm2_calc_secret(mbedtls_sm2_key_exchange_context *ctx, size_t *olen,
                             unsigned char *buf, size_t blen,
                             int (*f_rng)(void *, unsigned char *, size_t),
                             void *p_rng);

#if defined(MBEDTLS_ECP_RESTARTABLE)
/**
 * \brief           This function enables restartable EC computations for this
 *                  context.  (Default: disabled.)
 *
 * \see             \c mbedtls_ecp_set_max_ops()
 *
 * \note            It is not possible to safely disable restartable
 *                  computations once enabled, except by free-ing the context,
 *                  which cancels possible in-progress operations.
 *
 * \param ctx       The ECDH context to use. This must be initialized.
 */
void mbedtls_sm2_enable_restart(mbedtls_ecdh_context *ctx);
#endif /* MBEDTLS_ECP_RESTARTABLE */

int mbedtls_sm2_set_key(mbedtls_sm2_key_exchange_context *ctx, mbedtls_sm2_context *sm2_ctx);

int mbedtls_sm2_set_peerkey(mbedtls_sm2_key_exchange_context *ctx, mbedtls_sm2_context *sm2_ctx);

#ifdef __cplusplus
}
#endif

#endif /* sm2_key_exchange.h */