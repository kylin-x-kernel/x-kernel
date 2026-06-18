# scripts/make/hooks.mk
#
# Auto-enable the shared git hooks committed under .githooks/ so that a
# fresh `git clone` or a `git pull` needs no manual setup: the very first
# `make ...` command points git at .githooks/, and the hook file is kept
# up to date by subsequent pulls.
#
# Git deliberately cannot install hooks on clone/pull for safety reasons;
# wiring the bootstrap into Make parse time is the closest equivalent,
# because every developer in this project runs `make` to build or lint.
#
# Behavior (idempotent, runs at Make parse time):
#   - if core.hooksPath is unset, OR the configured path no longer has an
#     executable pre-commit hook (e.g. after the checkout was moved), set
#     it to <repo-toplevel>/.githooks;
#   - otherwise an existing, working custom hooksPath is left untouched.
#
# An absolute path is used for compatibility with git < 2.29, where a
# relative core.hooksPath is resolved against the current directory
# instead of the repository top level.

_GK_HOOKS_BOOTSTRAP := $(shell \
	top=$$(git rev-parse --show-toplevel 2>/dev/null); \
	if [ -n "$$top" ] && [ -d "$$top/.githooks" ]; then \
		want="$$top/.githooks"; \
		cur=$$(git config --get core.hooksPath 2>/dev/null); \
		if [ -z "$$cur" ] || [ ! -x "$$cur/pre-commit" ]; then \
			git config core.hooksPath "$$want" && \
				echo "x-kernel: enabled shared git hooks ($$want); see .githooks/pre-commit" >&2; \
		fi; \
	fi)

.PHONY: hooks
hooks: ## (Re)enable the shared git hooks (.githooks) for this clone.
	@git config core.hooksPath "$$(git rev-parse --show-toplevel)/.githooks"
	@echo "x-kernel: git hooks enabled -> $$(git config --get core.hooksPath)"
