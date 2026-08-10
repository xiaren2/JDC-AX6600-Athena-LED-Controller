module("luci.controller.athena_led", package.seeall)

local nixio = require "nixio"
local sys = require "luci.sys"
local http = require "luci.http"

function index()
    -- 如果配置文件不存在，就不显示菜单
   --  if not nixio.fs.access("/etc/config/athena_led") then
     --    return
 --    end

    -- ==============================
    -- 主菜单（单页面模式）
    -- ==============================
    entry({"admin", "services", "athena_led"},
        cbi("athena_led/settings"),
        _("Athena LED"), 60
    ).dependent = false

    -- ==============================
    -- 隐藏的状态查询接口 (AJAX API)
    -- ==============================
    entry({"admin", "services", "athena_led", "status"},
        call("act_status")
    ).leaf = true
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
