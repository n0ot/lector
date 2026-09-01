-- Copy this directory to the Lector configuration directory. On Unix that is
-- $XDG_CONFIG_HOME/lector, or $HOME/.config/lector when XDG_CONFIG_HOME is not
-- set to an absolute path.

local config_home = os.getenv("XDG_CONFIG_HOME")
if config_home == nil or config_home == "" or config_home:sub(1, 1) ~= "/" then
    config_home = assert(os.getenv("HOME"), "HOME is not set") .. "/.config"
end

local pager = dofile(config_home .. "/lector/pager.lua")

lector.bindings["M-P"] = {
    "read pager continuously",
    function()
        local ok, err = pcall(pager.read)
        if not ok then
            lector.api.speak("pager reader error: " .. tostring(err), true)
        end
    end,
}
