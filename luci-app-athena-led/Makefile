include $(TOPDIR)/rules.mk

PKG_NAME:=luci-app-athena-led
PKG_VERSION:=1.0.5
PKG_RELEASE:=1


PKG_MAINTAINER:=unraveloop <https://github.com/unraveloop>
PKG_LICENSE:=Apache-2.0
PKG_LICENSE_FILES:=LICENSE


PKG_SOURCE:=athena-led-aarch64-unknown-linux-musl-v$(PKG_VERSION).tar.gz


PKG_SOURCE_URL:=https://github.com/xiaren2/JDC-AX6600-Athena-LED-Controller/releases/download/v$(PKG_VERSION)/


PKG_HASH:=skip

include $(INCLUDE_DIR)/package.mk

define Package/$(PKG_NAME)
  SECTION:=luci
  CATEGORY:=LuCI
  SUBMENU:=3. Applications
  TITLE:=LuCI Support for Athena LED (Enhanced)

  DEPENDS:=+luci-base +rpcd @(aarch64||arm)
  PKGARCH:=all
endef

define Package/$(PKG_NAME)/description
  Enhanced LuCI support for JDCloud AX6600 (Athena) LED Screen Control.
  
  New Features in this version:
  - Real-time Network Speed (Upload/Download)
  - Zero-Load Precision Sleep Mode
  - ARP Online Device Count
  - WAN IP Display
  - System Monitor (CPU/RAM/Uptime)
  - Local Weather Integration
  - Stability Fixes for HTTP Text
endef

define Build/Prepare
	mkdir -p $(PKG_BUILD_DIR)
	$(CP) ./htdocs $(PKG_BUILD_DIR)/
	$(CP) ./root $(PKG_BUILD_DIR)/
	$(CP) ./po $(PKG_BUILD_DIR)/
endef

define Build/Compile
	[ -d ./po/zh_Hans ] && po2lmo ./po/zh_Hans/athena_led.po $(PKG_BUILD_DIR)/athena_led.zh-cn.lmo; true
	# 双后缀通杀: 不同固件 luci.i18n 会按 zh-cn / zh_Hans 不同后缀查翻译文件
	if [ -f $(PKG_BUILD_DIR)/athena_led.zh-cn.lmo ]; then \
		cp $(PKG_BUILD_DIR)/athena_led.zh-cn.lmo $(PKG_BUILD_DIR)/athena_led.zh_Hans.lmo; \
	fi; true
endef

define Package/$(PKG_NAME)/install
	# JS view 文件
	$(INSTALL_DIR) $(1)/www/luci-static/resources/view/athena_led
	$(INSTALL_DATA) ./htdocs/luci-static/resources/view/athena_led/settings.js $(1)/www/luci-static/resources/view/athena_led/

	# 菜单注册
	$(INSTALL_DIR) $(1)/usr/share/luci/menu.d
	$(INSTALL_DATA) ./root/usr/share/luci/menu.d/luci-app-athena-led.json $(1)/usr/share/luci/menu.d/

	# rpcd ACL 权限 (JS 版 LuCI 必须)
	$(INSTALL_DIR) $(1)/usr/share/rpcd/acl.d
	$(INSTALL_DATA) ./root/usr/share/rpcd/acl.d/luci-app-athena-led.json $(1)/usr/share/rpcd/acl.d/

	# init.d 启动脚本
	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) ./root/etc/init.d/athena_led $(1)/etc/init.d/

	# 配置文件
	$(INSTALL_DIR) $(1)/etc/config
	$(INSTALL_CONF) ./root/etc/config/athena_led $(1)/etc/config/

	# rc.button 按键脚本
	$(INSTALL_DIR) $(1)/etc/rc.button
	$(INSTALL_BIN) ./root/etc/rc.button/BTN_0 $(1)/etc/rc.button/

	# athena-led 二进制
	$(INSTALL_DIR) $(1)/usr/bin
	tar xf $(DL_DIR)/$(PKG_SOURCE) -C $(PKG_BUILD_DIR)
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/athena-led $(1)/usr/bin/

	# 国际化翻译 (JS 版 LuCI 路径) - 双后缀
	$(INSTALL_DIR) $(1)/usr/share/luci/i18n
	$(INSTALL_DATA) $(PKG_BUILD_DIR)/athena_led.zh-cn.lmo   $(1)/usr/share/luci/i18n/ 2>/dev/null; true
	$(INSTALL_DATA) $(PKG_BUILD_DIR)/athena_led.zh_Hans.lmo $(1)/usr/share/luci/i18n/ 2>/dev/null; true

	# 国际化翻译 (兼容旧版 Lua LuCI 路径)
	$(INSTALL_DIR) $(1)/usr/lib/lua/luci/i18n
	$(INSTALL_DATA) $(PKG_BUILD_DIR)/athena_led.zh-cn.lmo   $(1)/usr/lib/lua/luci/i18n/ 2>/dev/null; true
	$(INSTALL_DATA) $(PKG_BUILD_DIR)/athena_led.zh_Hans.lmo $(1)/usr/lib/lua/luci/i18n/ 2>/dev/null; true
endef

# 装完清缓存 + 重载 rpcd, 否则:
#   - LuCI 页面会白屏直到下次重启 (缓存未重建)
#   - ACL 权限不生效 (rpcd 未重读)
define Package/$(PKG_NAME)/postinst
#!/bin/sh
if [ -z "$${IPKG_INSTROOT}" ]; then
    rm -rf /tmp/luci-indexcache /tmp/luci-modulecache/* 2>/dev/null
    mkdir -p /tmp/luci-modulecache 2>/dev/null
    if command -v /etc/init.d/rpcd >/dev/null 2>&1; then
        /etc/init.d/rpcd reload >/dev/null 2>&1 || true
    fi
fi
exit 0
endef

$(eval $(call BuildPackage,$(PKG_NAME)))
