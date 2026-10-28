# Image tags: playpen-base + playpen-<flavor>. Flavors live in flavors/<name>/.
# Adding a flavor: new dir + Containerfile (FROM playpen-base:latest)
# + a rule below, tagged playpen-<name>. The launcher then takes it as-is:
#   playpen playpen-<name>

ENGINE ?= podman

.PHONY: all base rust jvm

all: base rust jvm

base:
	$(ENGINE) build -t playpen-base base/

rust: base
	$(ENGINE) build -t playpen-rust flavors/rust

jvm: base
	$(ENGINE) build -t playpen-jvm flavors/jvm
