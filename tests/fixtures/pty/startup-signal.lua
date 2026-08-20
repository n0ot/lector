local server = os.getenv("LECTOR_TEST_STARTUP_SIGNAL_SERVER")
local pid_file = os.getenv("LECTOR_TEST_STARTUP_SIGNAL_PID_FILE")

assert(server ~= nil)
assert(pid_file ~= nil)

lector.o.speech = {
    program = "/bin/bash",
    args = { server, pid_file },
}
