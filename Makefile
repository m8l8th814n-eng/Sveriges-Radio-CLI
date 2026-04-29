PREFIX ?= /usr/local
CARGO ?= cargo

.PHONY: all build release install clean

all: build

build:
	$(CARGO) build --bin srtui

release:
	$(CARGO) build --release --bin srtui

install: release
	install -Dm755 target/release/srtui $(DESTDIR)$(PREFIX)/bin/srtui

clean:
	$(CARGO) clean
