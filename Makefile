.PHONY: build package validate check fmt-check test clean

build: package

package:
	@tools/sh/pkgbuild_local.sh

validate: fmt-check check

check:
	@cargo check --workspace --locked
	@cargo test --workspace --locked
	@if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then git diff --check; fi

fmt-check:
	@cargo fmt --all -- --check

test:
	@cargo test --workspace --locked

clean:
	@cargo clean
	@rm -rf build/
