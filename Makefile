.PHONY: install-hooks fmt lint test build

## Install git hooks (run once after cloning)
install-hooks:
	git config core.hooksPath .githooks
	@echo "Git hooks installed (.githooks/pre-commit active)"

## Auto-format all packages
fmt:
	cargo fmt --all

## Lint all packages (warnings as errors)
lint:
	cargo clippy --workspace --all-targets -- -D warnings

## Run all tests
test:
	cargo test --workspace

## Build all packages (debug)
build:
	cargo build --workspace
