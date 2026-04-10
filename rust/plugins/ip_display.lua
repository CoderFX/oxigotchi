-- ip_display.lua: IP address display.
-- Rotates between BT IP and USB IP every 5 epochs when both available.
plugin = {}
plugin.name    = "ip_display"
plugin.version = "3.0.2"
plugin.author  = "oxigotchi"
plugin.tag     = "default"

local tick = 0

function on_load(config)
    register_indicator("ip_display", {
        x    = config.x,
        y    = config.y,
        font = "small",
    })
end

function on_epoch(state)
    tick = tick + 1
    if state.bt_connected and state.bt_ip ~= "" then
        if tick % 10 < 5 then
            set_indicator("ip_display", state.bt_ip)
        else
            set_indicator("ip_display", state.display_ip)
        end
    else
        set_indicator("ip_display", state.display_ip)
    end
end
