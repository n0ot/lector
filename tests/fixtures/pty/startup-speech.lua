local speech_server = os.getenv("LECTOR_TEST_STARTUP_SPEECH_SERVER")
local expected_config = os.getenv("LECTOR_TEST_STARTUP_CONFIG")

assert(speech_server ~= nil)
assert(expected_config ~= nil)

lector.o.speech = {
    program = speech_server,
    args = {
        "--legacy",
        "--startup-argv-probe",
        "argument with spaces",
        "'literal punctuation'",
        "$(opaque text)",
    },
}

-- This remains valid for compatibility with existing init.lua files. Lector
-- must buffer it until the configured process has completed its handshake.
lector.api.speak("LECTOR-TOP-LEVEL-SPEAK", false)

lector.hooks.on_startup = function(event)
    assert(event.config_path == expected_config)
    local order_log = os.getenv("LECTOR_TEST_STARTUP_ORDER_LOG")
    if order_log ~= nil then
        local log = assert(io.open(order_log, "a"))
        log:write("hook\n")
        log:close()
    end
    -- This synchronous marker shares Lector's physical PTY. Unlike the
    -- asynchronous speech request below, its byte ordering directly proves
    -- that the compositor frame was flushed before the hook ran.
    io.stderr:write("\27]2;LECTOR-STARTUP-HOOK-RAN\27\\")
    io.stderr:flush()
    lector.api.speak("LECTOR-STARTUP-HOOK-SPEAK", false)
end
