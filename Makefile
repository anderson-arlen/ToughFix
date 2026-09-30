# SPDX-License-Identifier: MIT
.DEFAULT_GOAL := build

PREFIX ?= $(HOME)/.local
DATA_DIR ?= $(if $(XDG_DATA_HOME),$(XDG_DATA_HOME),$(PREFIX)/share)
CONFIG_DIR ?= $(if $(XDG_CONFIG_HOME),$(XDG_CONFIG_HOME),$(HOME)/.config)
DESTDIR ?=
CAMERA_ACCESS ?= 1
# Quote paths as single shell arguments, including spaces and literal $/quotes.
quote = '$(subst ','"'"',$(1))'
INSTALL_ARGS = --prefix $(call quote,$(PREFIX)) --data-dir $(call quote,$(DATA_DIR)) --config-dir $(call quote,$(CONFIG_DIR)) --destdir $(call quote,$(DESTDIR)) --camera-access $(CAMERA_ACCESS)

ifneq ($(filter install install-user,$(MAKECMDGOALS)),)
ifeq ($(strip $(DESTDIR)),)
ifeq ($(shell id -u),0)
$(error Run make install as your desktop user; only camera setup requests sudo)
endif
endif
endif

.PHONY: build install install-user install-camera-access check help
build:
	cargo build --release --locked

install: build
	./target/release/toughfix install $(INSTALL_ARGS)

install-user:
	$(MAKE) install CAMERA_ACCESS=0

install-camera-access: build
	./target/release/toughfix install --camera-only --destdir $(call quote,$(DESTDIR))

check:
	cargo test --locked
	cargo clippy --locked --all-targets -- -D warnings

help:
	@echo 'make install                 Build/install/update ToughFix; sudo for first camera setup'
	@echo 'make install CAMERA_ACCESS=0 Install/update user files without system changes'
	@echo 'Camera connection starts ToughFix; change this setting in the app'
	@echo 'make install-camera-access  Install udev access and load the sg driver only'
	@echo 'make check                   Rust checks and installer regression tests'
	@echo 'PREFIX, DATA_DIR, CONFIG_DIR, DESTDIR customize installation paths'
