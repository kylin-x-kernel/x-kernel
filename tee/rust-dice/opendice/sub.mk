incdirs-y += dice/include

srcs-y += pta_dice.c
srcs-y += tee_dice.c
srcs-y += dice/src/cbor_reader.c
srcs-y += dice/src/cbor_writer.c
srcs-y += dice/src/clear_memory.c
ifeq ($(CFG_USING_DICE_CHAIN), y)
	cflags-y  += -DCFG_DICE_CHAIN

	srcs-y += dice/src/android.c
	srcs-y += dice/src/boringssl_p256_ops.c
	srcs-y += dice/src/cbor_cert_op.c
	srcs-y += dice/src/dice.c
	srcs-y += dice/src/mbedtls_ops.c
	srcs-y += dice/src/utils.c
	srcs-y += dice/src/tee_ecdsa_utils.c
endif