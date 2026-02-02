#!/bin/bash
DIR = ../../../build/programs
DIR_EXISTS := $(shell if [ -d "test" ]; then echo "1"; else echo "0"; fi)

main:
	@if [ $(DIR_EXISTS) -eq 0 ]; then \
		mkdir test; \
	fi
	@cd test && \
	$(DIR)/pkey/gen_key type=ec ec_curve=sm2p256r1 filename=ca_key.pem && \
	$(DIR)/x509/cert_write selfsign=1 issuer_key=ca_key.pem issuer_name="CN=My Root CA,O=My Organization,C=CN" not_before=20240101000000 not_after=20350101000000 output_file=ca_cert.pem md=SM3 is_ca=1 basic_constraints=1   key_usage=key_cert_sign,crl_sign ns_cert_type=ssl_ca && \
	$(DIR)/pkey/gen_key type=ec ec_curve=sm2p256r1 filename=server_key.pem && \
	$(DIR)/x509/cert_req filename=server_key.pem output_file=server_csr.pem subject_name="CN=server,O=My Organization,C=CN" md=SM3 && \
	$(DIR)/x509/cert_write issuer_key=ca_key.pem issuer_name="CN=My Root CA,O=My Organization,C=CN" request_file=server_csr.pem ns_cert_type=ssl_server subject_name="CN=server,O=My Organization,C=CN" output_file=server_cert.pem not_before=20240101000000 not_after=20320101000000 md=SM3 key_usage=digital_signature,key_encipherment && \
	$(DIR)/pkey/gen_key type=ec ec_curve=sm2p256r1 filename=client_key.pem && \
	$(DIR)/x509/cert_req filename=client_key.pem output_file=client_csr.pem subject_name="CN=client,O=My Organization,C=CN" md=SM3 ns_cert_type=ssl_client && \
	$(DIR)/x509/cert_write issuer_key=ca_key.pem issuer_name="CN=My Root CA,O=My Organization,C=CN" request_file=client_csr.pem subject_name="CN=client,O=My Organization,C=CN" output_file=client_cert.pem not_before=20240101000000 not_after=20300101000000 md=SM3 ns_cert_type=ssl_client

check:
	@cd test && \
	echo "\n/************/\n验证服务器证书\n/************/" && \
	$(DIR)/x509/cert_app mode=file filename=server_cert.pem ca_file=ca_cert.pem && \
	echo "\n/************/\n验证客户端证书\n/************/" && \
	$(DIR)/x509/cert_app mode=file filename=client_cert.pem ca_file=ca_cert.pem

clean:
	@cd test && \
	rm -rf *
	@rm -rf test
