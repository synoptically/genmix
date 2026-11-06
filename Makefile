BIN := bin/genmix
PREFIX ?= $(HOME)/.local
DESKTOP := org.genmix.GenMix.desktop

.PHONY: all install uninstall test clean

all: $(BIN)

$(BIN): Cargo.toml Cargo.lock $(shell find src -name '*.rs')
	cargo build --release --locked
	install -D target/release/genmix $@

install: $(BIN)
	install -Dm755 $(BIN) $(DESTDIR)$(PREFIX)/bin/genmix
	install -Dm644 data/$(DESKTOP) $(DESTDIR)$(PREFIX)/share/applications/$(DESKTOP)

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/genmix $(DESTDIR)$(PREFIX)/share/applications/$(DESKTOP)

test:
	cargo test --locked

clean:
	cargo clean
	rm -rf bin
