# Image tags: playpen-base + playpen-<flavor>. Flavors live in flavors/<name>/.
# Adding a flavor: new dir + Containerfile (FROM localhost/playpen-base:latest)
# + a rule below, tagged playpen-<name>. The launcher then takes it as-is:
#   playpen playpen-<name>

.PHONY: all base rust jvm

all: base rust jvm

base:
	podman build -t playpen-base base/

rust: base
	podman build -t playpen-rust flavors/rust

jvm: base
	podman build -t playpen-jvm flavors/jvm
