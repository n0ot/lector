local callbacks = ...

local function set_symbol(symbol, replacement_def)
    if type(symbol) ~= "string" then
        error("symbol name must be a string", 2)
    end
    -- replacement_def can be nil to remove a symbol, or a table to set/update it.
    if replacement_def ~= nil and (type(replacement_def) ~= 'table' or #replacement_def < 4) then
        error("replacement_def must be a table {replacement, level, include_original, repeat} or nil to remove", 2)
    end
    callbacks.set_symbol(symbol, replacement_def)
end

local function set_binding(key, binding_def)
    if type(key) ~= "string" then
        error("binding key must be a string", 2)
    end
    if binding_def ~= nil then
        local t = type(binding_def)
        if t ~= "table" and t ~= "string" then
            error("binding value must be a string, table, or nil", 2)
        end
        if t == "table" then
            local help = binding_def.help or binding_def[1]
            local fn = binding_def.fn or binding_def[2]
            if type(help) ~= "string" or type(fn) ~= "function" then
                error("binding table must be {help, fn} or {help=..., fn=...}", 2)
            end
        end
    end
    if callbacks.set_binding == nil then
        error("bindings are not available in this context", 2)
    end
    callbacks.set_binding(key, binding_def)
end

local tbl_lector_symbols_mt = {
    __index = function(_, k)
        if type(k) ~= "string" then
            error("symbol name must be a string for indexing", 2)
        end
        return callbacks.get_symbol(k)
    end,
    __newindex = function(_, k, v)
        return set_symbol(k, v)
    end,
}
local tbl_lector_symbols = setmetatable({}, tbl_lector_symbols_mt)

local tbl_lector_bindings_mt = {
    __index = function(_, k)
        if type(k) ~= "string" then
            error("binding key must be a string for indexing", 2)
        end
        if callbacks.get_binding == nil then
            error("bindings are not available in this context", 2)
        end
        return callbacks.get_binding(k)
    end,
    __newindex = function(_, k, v)
        return set_binding(k, v)
    end,
}
local tbl_lector_bindings = setmetatable({}, tbl_lector_bindings_mt)

local tbl_lector_o_mt = {
    __index = function(_, k)
        if type(k) ~= "string" then
            error("option name must be a string for indexing", 2)
        end
        return callbacks.get_option(k)
    end,
    __newindex = function(_, k, v)
        if type(k) ~= "string" then
            error("option name must be a string for assignment", 2)
        end
        callbacks.set_option(k, v)
    end,
}
local tbl_lector_o = setmetatable({}, tbl_lector_o_mt)

local tbl_lector_mt = {
    __index = function(t, k)
        if k == 'o' then
            return tbl_lector_o
        elseif k == 'symbols' then
            return tbl_lector_symbols
        elseif k == 'bindings' then
            return tbl_lector_bindings
        else
            return rawget(t, k)
        end
    end,
    __newindex = function(_, k, v)
        if k == "symbols" then
            if type(v) ~= "table" then
                error("lector.symbols must be assigned a table", 2)
            end

            for symbol, replacement_def in pairs(v) do
                if type(symbol) ~= "string" then
                    error("lector.symbols table keys must be strings. Found key of type: " .. type(symbol), 2)
                end
                if type(replacement_def) ~= 'table' or #replacement_def < 4 then
                    error(
                        "lector.symbols table values must be tables of format {replacement, level, include_original, repeat}. Error at key: '" ..
                        tostring(symbol) .. "'", 2)
                end
            end

            callbacks.clear_symbols()

            for symbol, replacement_def in pairs(v) do
                callbacks.set_symbol(symbol, replacement_def)
            end

            tbl_lector_symbols = setmetatable(v, tbl_lector_symbols_mt)
        elseif k == "bindings" then
            error("assign individual bindings via lector.bindings[key] = value", 2)
        else
            error("cannot assign to arbitrary keys on the lector table", 2)
        end
    end,
}
setmetatable(lector, tbl_lector_mt)
