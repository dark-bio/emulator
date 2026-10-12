.DEFAULT_GOAL := qemu-linux

QEMU_OUTPUT ?= $(CURDIR)/target/qemu-linux
QEMU_JOBS ?= 4
QEMU_MACOS_TARGET = $(if $(filter arm64,$(shell uname -m)),macos-arm64,macos-amd64)

.PHONY: qemu-macos qemu-macos-check qemu-windows qemu-windows-check
qemu-macos qemu-macos-check: QEMU_OUTPUT = $(CURDIR)/target/qemu-macos
qemu-windows qemu-windows-check: QEMU_OUTPUT = $(CURDIR)/target/qemu-windows

qemu-macos:
	cargo run --locked -p qemu-build -- build --target $(QEMU_MACOS_TARGET) --repo "$(CURDIR)" --output "$(QEMU_OUTPUT)" --jobs "$(QEMU_JOBS)"

qemu-macos-check:
	cargo run --locked -p qemu-build -- check --target $(QEMU_MACOS_TARGET) --repo "$(CURDIR)" --output "$(QEMU_OUTPUT)"

qemu-windows:
	cargo run --locked -p qemu-build -- build --target windows-amd64 --repo "$(CURDIR)" --output "$(QEMU_OUTPUT)" --jobs "$(QEMU_JOBS)"

qemu-windows-check:
	cargo run --locked -p qemu-build -- check --target windows-amd64 --repo "$(CURDIR)" --output "$(QEMU_OUTPUT)"

.PHONY: qemu-linux qemu-linux-check

qemu-linux:
	cargo run --locked -p qemu-build -- build --target linux-amd64 --repo "$(CURDIR)" --output "$(QEMU_OUTPUT)" --jobs "$(QEMU_JOBS)"

qemu-linux-check:
	cargo run --locked -p qemu-build -- check --target linux-amd64 --repo "$(CURDIR)" --output "$(QEMU_OUTPUT)"
