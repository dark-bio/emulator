QEMU_OUTPUT ?= $(CURDIR)/target/qemu-linux
QEMU_JOBS ?= 4

.PHONY: qemu-linux qemu-linux-check

qemu-linux:
	cargo run --locked -p qemu-build -- build --repo "$(CURDIR)" --output "$(QEMU_OUTPUT)" --jobs "$(QEMU_JOBS)"

qemu-linux-check:
	cargo run --locked -p qemu-build -- check --repo "$(CURDIR)" --output "$(QEMU_OUTPUT)"
