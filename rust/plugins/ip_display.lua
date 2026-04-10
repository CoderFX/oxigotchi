-- ip_display.lua: IP address display.
-- Shows BT IP when connected, falls back to USB IP.
plugin = {}
plugin.name    = "ip_display"
plugin.version = "3.0.1"
plugin.author  = "oxigotchi"
plugin.tag     = "default"

function on_load(config)
    register_indicator("ip_display", {
        x    = config.x,
        y    = config.y,
        font = "small",
    })
end

function on_epoch(state)
    if state.bt_connected and state.bt_ip ~= "" then
        set_indicator("ip_display", state.bt_ip)
    else
        set_indicator("ip_display", state.display_ip)
    end
end
