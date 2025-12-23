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
        else
            error("cannot assign to arbitrary keys on the lector table", 2)
        end
    end,
}
setmetatable(lector, tbl_lector_mt)
