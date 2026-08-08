PREFIX ?= $(HOME)/.config/rift/bin
BINARY := rift-ergo
RELEASE_BINARY := target/release/$(BINARY)

.PHONY: build check clean fmt install test uninstall

build:
	cargo build --release --locked

fmt:
	cargo fmt

test:
	cargo test --locked

check:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --locked

install: build
	mkdir -p "$(PREFIX)"
	install -m 755 "$(RELEASE_BINARY)" "$(PREFIX)/$(BINARY).new"
	mv "$(PREFIX)/$(BINARY).new" "$(PREFIX)/$(BINARY)"

uninstall:
	rm -f "$(PREFIX)/$(BINARY)"

clean:
	cargo clean
