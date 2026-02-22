module("luci.controller.athena_led", package.seeall)

local nixio = require "nixio"
local sys = require "luci.sys"
local http = require "luci.http"

function index()
    -- 如果配置文件不存在，就不显示菜单
   --  if not nixio.fs.access("/etc/config/athena_led") then
     --    return
 --    end

    -- 1. 主菜单入口
    -- 我把它改到了 "Services" (服务) 下，这样更符合 OpenWrt 插件规范
    -- firstchild() 表示点击这个菜单时，自动跳到第一个子菜单(Settings)
    entry({"admin", "services", "athena_led"}, firstchild(), _("Athena LED"), 60).dependent = false

    -- 2. 设置页面 (Settings)
    -- 指向 model/cbi/athena_led/settings.lua
    entry({"admin", "services", "athena_led", "settings"}, cbi("athena_led/settings"), _("Base Setting"), 1)

    -- 3. 隐藏的状态查询接口 (Status API)
    -- 前端可以通过 AJAX 请求这个地址来获取运行状态
    entry({"admin", "services", "athena_led", "status"}, call("act_status")).leaf = true
end

function act_status()
    local e = {}

    -- 只匹配进程名，不匹配命令行
    local pid = sys.exec("pidof athena-led 2>/dev/null")
    pid = pid:gsub("\n", "")

    if pid ~= "" then
        -- 如果有多个 PID，只取第一个
        pid = pid:match("^(%d+)")
        e.running = true
        e.pid = pid
    else
        e.running = false
        e.pid = nil
    end

    http.prepare_content("application/json")
    http.write_json(e)
end
