local callbacks = ...

local function make_accessor(get, set)
    local mt = {}

    function mt:__newindex(k, v)
        return set(k, v)
    end

    function mt:__index(k)
        return get(k)
    end

    return setmetatable({}, mt)
end

lector.o = make_accessor(
    function(k) return callbacks.get_option(k) end,
    function(k, v) return callbacks.set_option(k, v) end
)

lector.symbols = make_accessor(
    function(k) return callbacks.get_symbol(k) end,
    function(k, v)
        if type(k) ~= "string" then
            error("symbol name must be a string", 2)
        end
        if type(v) ~= 'table' or #v < 4 then
            error("value must be a symbol desc containing {replacement, level, include_original, repeat}", 2)
        end
        return callbacks.set_symbol(k, v)
    end
)
