# Image tags: omp-base + omp-<flavor>. Flavors live in flavors/<name>/.
# Adding a flavor: new dir + Containerfile (FROM localhost/omp-base:latest)
# + a rule below, tagged omp-<name>. The launcher then takes it as-is:
#   omp-sbx omp-<name>

.PHONY: all base rust jvm

all: base rust jvm

base:
	podman build -t omp-base base/

rust: base
	podman build -t omp-rust flavors/rust

jvm: base
	podman build -t omp-jvm flavors/jvm
