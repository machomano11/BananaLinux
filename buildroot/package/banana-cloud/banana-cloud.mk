################################################################################
#
# banana-cloud
#
################################################################################

BANANA_CLOUD_VERSION = local
BANANA_CLOUD_SITE = $(BR2_EXTERNAL_BANANA_PATH)/../system/banana-cloud
BANANA_CLOUD_SITE_METHOD = local
# Slot's slot-cloud and slot-net (system/banana-cloud/slot) are GPL-3.0-or-later, so the program is too
BANANA_CLOUD_LICENSE = GPL-3.0+
BANANA_CLOUD_LICENSE_FILES = slot/LICENSE

# A local package has no download step, so cargo's vendoring (done after a download) never runs: the crates in
# Cargo.lock are fetched here instead, into Buildroot's cargo home in the download folder, kept for the next build.
define BANANA_CLOUD_BUILD_CMDS
	cd $(@D) && $(TARGET_MAKE_ENV) $(TARGET_CONFIGURE_OPTS) $(PKG_CARGO_ENV) \
		cargo build --release --locked --manifest-path Cargo.toml
endef

define BANANA_CLOUD_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 755 $(@D)/target/$(RUSTC_TARGET_NAME)/release/banana-cloud $(TARGET_DIR)/usr/bin/banana-cloud
endef

$(eval $(cargo-package))
