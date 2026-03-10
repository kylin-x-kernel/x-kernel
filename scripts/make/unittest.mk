EXTRACT_COV = env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo run -p extract_cov --
CONVERT_COV = env RUSTFLAGS= CARGO_ENCODED_RUSTFLAGS= cargo run -p convert_cov --
out_target := $(TARGET_DIR)/$(TARGET)/$(MODE)
cov_profraw := $(out_target)/default.profraw
cov_prodata := $(out_target)/default.profdata
cov_export := $(out_target)/coverage.info
cov_text := $(out_target)/coverage.txt
cov_html := $(out_target)/coverage.html
cov_xml := $(out_target)/coverage.xml

ifeq ($(UNITTEST), y)
  RUSTFLAGS += --cfg unittest --check-cfg cfg(unittest) \
                -C instrument-coverage \
                -Z no-profiler-runtime
  APP_FEAT += unittest
else
  RUSTFLAGS += --check-cfg cfg(unittest)
endif

ifeq ($(ARCH), x86_64)
  CFLAGS_x86_64_unknown_none += -mcmodel=large
  export CFLAGS_x86_64_unknown_none
endif

define coverage_report
  @printf "    $(CYAN_C)Generating$(END_C) coverage report...\n"
  @$(EXTRACT_COV) --image $(DISK_IMG) --profraw-path /.llvm-cov/default.profraw --out-path $(cov_profraw)
  @printf "    $(CYAN_C)Extracted$(END_C) raw coverage data to $(notdir $(cov_profraw)).\n"
  @rust-profdata merge -o $(cov_prodata) $(cov_profraw)
  @printf "    $(CYAN_C)Merged$(END_C) raw coverage data into $(notdir $(cov_text)).\n"
  @rust-cov report $(OUT_ELF) --instr-profile=$(cov_prodata) --ignore-filename-regex='/.cargo/registry' > $(cov_text)
  @printf "    $(CYAN_C)Generated$(END_C) text coverage report at $(notdir $(cov_export)).\n"
  @rust-cov export $(OUT_ELF) --instr-profile=$(cov_prodata) --format=lcov --ignore-filename-regex='/.cargo/registry' > $(cov_export)
  @printf "    $(CYAN_C)Exported$(END_C) lcov data to $(notdir $(cov_xml)).\n"
  @$(CONVERT_COV) $(cov_export) $(cov_xml) --base-dir $(CURDIR)
  @printf "    $(CYAN_C)Finished$(END_C) generating Cobertura XML at $(notdir $(cov_xml)).\n"
endef