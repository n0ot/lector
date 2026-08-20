local speech_server = os.getenv("LECTOR_TEST_SPEECH_SERVER")
local lifecycle_state = os.getenv("LECTOR_TEST_FATAL_SPEECH_STATE")
local rpc_log = os.getenv("LECTOR_TEST_FATAL_SPEECH_RPC_LOG")

assert(speech_server ~= nil)
assert(lifecycle_state ~= nil)
assert(rpc_log ~= nil)

lector.o.speech = {
    program = speech_server,
    args = {
        "--lifecycle-state", lifecycle_state,
        "--rpc-log", rpc_log,
        "--crash-speak-generations", "1,2",
    },
}

-- Generation 1 crashes while deferred startup speech is drained. The
-- supervisor may restart it because no recent crash has occurred.
lector.api.speak("LECTOR-FIRST-CRASH", false)

lector.hooks.on_startup = function(_)
    -- Generation 2 crashes inside the 30-second window. This must wake the
    -- main loop and terminate Lector instead of silently disabling speech.
    lector.api.speak("LECTOR-SECOND-CRASH", false)
end
