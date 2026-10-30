# Image tags: playpen-base + playpen-<flavor>. Flavors live in flavors/<name>/.
# Adding a flavor: new dir + Containerfile (FROM playpen-base:latest)
# + a rule below, tagged playpen-<name>. The launcher then takes it as-is:
#   playpen playpen-<name>

ENGINE ?= podman
CARGO ?= cargo
LAUNCHER ?= launcher

# The install path must follow cargo's artifact layout. CARGO_TARGET_DIR and
# CARGO_BUILD_TARGET (from the environment) move the binary; they are passed
# to cargo explicitly so build and copy always agree. cargo's config files
# (build.target-dir) are not visible here - set these via the environment.
CARGO_TARGET_DIR ?= $(LAUNCHER)/target
CARGO_TARGET := $(if $(CARGO_BUILD_TARGET),--target $(CARGO_BUILD_TARGET),)
TRIPLE_DIR := $(if $(CARGO_BUILD_TARGET),$(CARGO_BUILD_TARGET)/,)

.PHONY: all base rust jvm test install

all: base rust jvm

base:
	$(ENGINE) build -t playpen-base -f base/Containerfile base/

rust: base
	$(ENGINE) build -t playpen-rust -f flavors/rust/Containerfile flavors/rust

jvm: base
	$(ENGINE) build -t playpen-jvm -f flavors/jvm/Containerfile flavors/jvm

test:
	$(CARGO) test --manifest-path $(LAUNCHER)/Cargo.toml

install:
	$(CARGO) build --release --manifest-path $(LAUNCHER)/Cargo.toml \
	  --target-dir "$(CARGO_TARGET_DIR)" $(CARGO_TARGET)
	mkdir -p $(HOME)/.local/bin
	install -m 755 "$(CARGO_TARGET_DIR)/$(TRIPLE_DIR)release/playpen" \
	  "$(HOME)/.local/bin/playpen"
