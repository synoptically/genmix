BIN := bin/genmix

.PHONY: all test clean

all: $(BIN)

$(BIN): Cargo.toml Cargo.lock $(shell find src -name '*.rs')
	cargo build --release --locked
	install -D target/release/genmix $@

test:
	cargo test --locked

clean:
	cargo clean
	rm -rf bin
