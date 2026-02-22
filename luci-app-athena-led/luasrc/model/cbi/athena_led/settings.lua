local sys = require "luci.sys"

m = Map("athena_led",
    translate("Athena LED Controller"),
    translate("JDCloud AX6600 LED Screen Ctrl")
)

m:section(SimpleSection).template = "athena_led/athena_led_status"

-- ⚠️ 必须匹配 UCI: config settings 'general'
s = m:section(NamedSection, "general", "settings")
s.anonymous = true
s.addremove = false

-- Tabs
s:tab("general", translate("General Settings"))
s:tab("network", translate("Network Settings"))
s:tab("sensor", translate("Sensor & Weather"))
s:tab("custom", translate("Custom Content"))
s:tab("sleep", translate("Scheduled Sleep"))
s:tab("service", translate("Service Control"))

-- ================= GENERAL =================
o = s:taboption("general", Flag, "enabled", translate("Enabled"))
o.rmempty = false

o = s:taboption("general", ListValue, "light_level", translate("Brightness Level"))
o.default = "5"
for i=0,7 do o:value(i) end
o.description = translate("Adjust brightness (0-7).")

o = s:taboption("general", Value, "duration", translate("Loop Interval (s)"))
o.datatype = "uinteger"
o.default = "5"
o.description = translate("Time in seconds to display each module.")

o = s:taboption("general", DynamicList, "display_order", translate("Display Order & Modules"))
o.description = translate("Add modules and drag to reorder.")
o:value("date", translate("Date (MM-DD)"))
o:value("time", translate("Time (HH:MM)"))
o:value("timeBlink", translate("Time (Blink)"))
o:value("uptime", translate("System Uptime"))
o:value("weather", translate("Weather"))
o:value("cpu", translate("CPU Load"))
o:value("mem", translate("RAM Usage"))
o:value("temp", translate("Temperatures"))
o:value("ip", translate("WAN IP"))
o:value("dev", translate("Online Devices (ARP)"))
o:value("netspeed_down", translate("Realtime Speed (RX)"))
o:value("netspeed_up", translate("Realtime Speed (TX)"))
o:value("traffic_down", translate("Total Traffic (RX)"))
o:value("traffic_up", translate("Total Traffic (TX)"))
o:value("banner", translate("Custom Text"))
o:value("http_custom", translate("HTTP Request Result"))

-- ================= NETWORK =================
o = s:taboption("network", Value, "net_interface", translate("Network Interface"))
o.default = "br-lan"
for _, dev in ipairs(sys.net.devices()) do
    if dev ~= "lo" then o:value(dev) end
end

o = s:taboption("network", Value, "wan_ip_custom_url", translate("WAN IP API"))
o:value("http://checkip.amazonaws.com", "Amazon AWS")
o:value("http://ifconfig.me/ip", "ifconfig.me")
o:value("http://ipv4.icanhazip.com", "icanhazip.com")

-- ================= SENSOR =================
o = s:taboption("sensor", MultiValue, "temp_sensors", translate("Temperature Sensors"))
o.widget = "checkbox"
o:value("0", "nss-top")
o:value("1", "nss")
o:value("2", "wcss-phya0")
o:value("3", "wcss-phya1")
o:value("4", "cpu")
o:value("5", "lpass")
o:value("6", "ddrss")

o = s:taboption("sensor", ListValue, "weather_source", translate("Weather Source"))
o:value("wttr", "Wttr.in")
o:value("openmeteo", "Open-Meteo")
o:value("seniverse", "Seniverse")
o:value("uapis", "Uapis.cn")

o = s:taboption("sensor", Value, "weather_city", translate("City Name"))
o.default = "Shenzhen"

o = s:taboption("sensor", Value, "seniverse_key", translate("Seniverse API Key"))
o:depends("weather_source", "seniverse")

o = s:taboption("sensor", ListValue, "weather_format", translate("Weather Format"))
o:value("simple", translate("Simple (Icon + Temp)"))
o:value("full", translate("Full (Original)"))

-- ================= CUSTOM =================
o = s:taboption("custom", Value, "custom_content", translate("Custom Text"))
o.placeholder = "Roc-Gateway"

o = s:taboption("custom", Value, "http_url", translate("HTTP Request URL"))

o = s:taboption("custom", Value, "http_length", translate("HTTP Max Length"))
o.datatype = "uinteger"
o.default = "15"

-- ================= SLEEP =================
o = s:taboption("sleep", Flag, "enable_sleep", translate("Enable Scheduled Sleep"))

o = s:taboption("sleep", Value, "off_time", translate("Screen Off Time"))
o:depends("enable_sleep", "1")

o = s:taboption("sleep", Value, "on_time", translate("Screen On Time"))
o:depends("enable_sleep", "1")

-- ================= SERVICE =================
btn_restart = s:taboption("service", Button, "_restart", translate("Restart Service"))
btn_restart.inputstyle = "apply"
function btn_restart.write(self, section)
    luci.sys.call("/etc/init.d/athena_led restart >/dev/null 2>&1")
end

btn_stop = s:taboption("service", Button, "_stop", translate("Stop Service"))
btn_stop.inputstyle = "remove"
function btn_stop.write(self, section)
    luci.sys.call("/etc/init.d/athena_led stop >/dev/null 2>&1")
end

return m
