#!/bin/bash

PROGRAM_PATH=../../programs/

${PROGRAM_PATH}/pkey/gen_key type=ec ec_curve=sm2p256r1 filename=ca_privkey.pem format=pem
${PROGRAM_PATH}/x509/cert_write selfsign=1 issuer_name=CN=CA,O=security,C=china issuer_key=ca_privkey.pem output_file=ca_cert.pem not_before=20180101000000 not_after=20491228000000 is_ca=1 md=SM3
${PROGRAM_PATH}/pkey/gen_key type=ec ec_curve=sm2p256r1  filename=bob_privkey.pem format=pem
${PROGRAM_PATH}/x509/cert_req filename=bob_privkey.pem subject_name=CN=Bob1,O=security1,C=china23 output_file=bob_cert.req md=SM3
${PROGRAM_PATH}/x509/cert_write request_file=bob_cert.req output_file=bob_cert.pem  not_before=20180101000000 not_after=20820101000000  issuer_key=ca_privkey.pem issuer_name=CN=CA,O=security,C=china md=SM3
${PROGRAM_PATH}/x509/cert_app mode=file filename=bob_cert.pem  ca_file=ca_cert.pem

#${PROGRAM_PATH}/pkey/gen_key type=ec ec_curve=sm2p256r1 filename=enc_privkey.pem format=pem
#gmssl ec -pubout -in enc_privkey.pem -out enc_pubkey.der -config ~/temp/GmSSL-master/apps/openssl.cnf

${PROGRAM_PATH}/pkey/pk_encrypt enc_pubkey.der "plain data: abc"
${PROGRAM_PATH}/pkey/pk_decrypt enc_privkey.pem

${PROGRAM_PATH}/pkey/pk_sign enc_privkey.pem bob_cert.req
${PROGRAM_PATH}/pkey/pk_verify enc_pubkey.der bob_cert.req