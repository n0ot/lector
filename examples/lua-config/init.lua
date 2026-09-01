local pager = require("pager")

lector.bindings["M-P"] = {
    "read pager continuously",
    function()
        local ok, err = pcall(pager.read)
        if not ok then
            lector.api.speak("pager reader error: " .. tostring(err), true)
        end
    end,
}
