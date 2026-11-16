# Local packaging targets. They run the same steps as .github/workflows/release.yml.
#
#   make dist            archive + .deb + .rpm + SHA256SUMS for this machine's target
#   make package-deb     target/debian/anchor-context_<ver>-1_<arch>.deb  (needs cargo-deb)
#   make package-rpm     target/generate-rpm/anchor-context-<ver>-1.<arch>.rpm (needs cargo-generate-rpm)
#   make apt-repo        target/apt-repo/ (unsigned unless GPG_KEY_ID is set)
#   make rpm-repo        target/rpm-repo/ (needs createrepo_c; signed repomd.xml when GPG_KEY_ID set)
#
# Tools: cargo install cargo-deb cargo-generate-rpm   (and dpkg-dev / createrepo_c for the repos)
# Set TARGET to cross-build, e.g. `make package-deb TARGET=aarch64-unknown-linux-gnu`.

SHELL := /bin/bash
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
HOST := $(shell rustc -vV | sed -n 's/^host: //p')
TARGET ?= $(HOST)
CARGO ?= cargo
RELEASE_DIR := target/$(TARGET)/release
DIST := target/dist
ARCHIVE := anchor-$(VERSION)-$(TARGET)

.PHONY: test-apt test-rpm build assets archive package-deb package-rpm apt-repo rpm-repo dist checksums clean-dist

build:
	$(CARGO) build --release --locked --target $(TARGET)

# Completions and the man page come from the binary itself, so they can only be generated for a
# target this machine can run; for a cross build the host binary produces identical text.
assets: build
	packaging/gen-assets.sh $(if $(filter $(TARGET),$(HOST)),$(RELEASE_DIR)/anchor,target/release/anchor)

archive: build
	rm -rf $(DIST)/$(ARCHIVE) && mkdir -p $(DIST)/$(ARCHIVE)
	cp $(RELEASE_DIR)/anchor README.md $(DIST)/$(ARCHIVE)/
	for f in LICENSE LICENSE.md LICENSE-MIT LICENSE-APACHE; do [ -f $$f ] && cp $$f $(DIST)/$(ARCHIVE)/ || true; done
	tar -C $(DIST) -czf $(DIST)/$(ARCHIVE).tar.gz $(ARCHIVE)
	cd $(DIST) && sha256sum $(ARCHIVE).tar.gz > $(ARCHIVE).tar.gz.sha256
	rm -rf $(DIST)/$(ARCHIVE)

package-deb: assets
	cargo deb --no-build --target $(TARGET) --output $(DIST)/
	@ls -l $(DIST)/*.deb

package-rpm: assets
	cargo generate-rpm --target $(TARGET) --output $(DIST)/
	@ls -l $(DIST)/*.rpm

apt-repo:
	packaging/apt/build-repo.sh target/apt-repo $(DIST)/*.deb

rpm-repo:
	packaging/rpm/build-repo.sh target/rpm-repo $(DIST)/*.rpm

checksums:
	cd $(DIST) && sha256sum *.tar.gz *.zip *.deb *.rpm 2>/dev/null > SHA256SUMS; cat SHA256SUMS

dist: archive package-deb package-rpm checksums

clean-dist:
	rm -rf $(DIST) target/apt-repo target/rpm-repo target/assets

# Install the packages in clean containers (needs Docker; builds an unsigned repo first).
test-apt: apt-repo
	for img in debian:bookworm-slim ubuntu:24.04; do docker run --rm -v $(CURDIR)/target/apt-repo:/repo -v $(CURDIR)/packaging/test/deb-in-container.sh:/t.sh:ro $$img /t.sh trusted || exit 1; done

test-rpm:
	docker run --rm -v $(CURDIR)/$(DIST):/in:ro -v $(CURDIR)/packaging:/pkg:ro fedora:latest /pkg/test/rpm-in-container.sh
