use std::ffi::c_void;

unsafe extern "C" {
    fn lector_visit_eh_frames(
        frames: *const u8,
        visit: unsafe extern "C" fn(*const u8, *mut c_void),
        context: *mut c_void,
    );
}

unsafe extern "C" fn collect(frame: *const u8, context: *mut c_void) {
    // SAFETY: visited() passes a live mutable vector for this synchronous call.
    let frames = unsafe { &mut *context.cast::<Vec<*const u8>>() };
    frames.push(frame);
}

fn visited(bytes: &[u8]) -> Vec<usize> {
    let mut frames: Vec<*const u8> = Vec::new();
    // SAFETY: each test supplies a complete zero-terminated DWARF table or
    // one complete FDE. The input and callback context outlive the traversal.
    unsafe {
        lector_visit_eh_frames(bytes.as_ptr(), collect, (&raw mut frames).cast());
    }
    frames
        .into_iter()
        .map(|frame| frame.addr() - bytes.as_ptr().addr())
        .collect()
}

#[test]
fn tables_visit_only_frame_descriptions_and_stop_at_the_terminator() {
    let table = [
        4, 0, 0, 0, 0, 0, 0, 0, // common information
        4, 0, 0, 0, 12, 0, 0, 0, // frame description
        4, 0, 0, 0, 0, 0, 0, 0, // another common information entry
        4, 0, 0, 0, 12, 0, 0, 0, // another frame description
        0, 0, 0, 0, // terminator
        4, 0, 0, 0, 12, 0, 0, 0, // outside the table
    ];
    assert_eq!(visited(&table), [8, 24]);
    assert!(visited(&[0, 0, 0, 0]).is_empty());
}

#[test]
fn single_frame_registration_does_not_read_past_the_frame() {
    assert_eq!(visited(&[4, 0, 0, 0, 8, 0, 0, 0]), [0]);
}

#[test]
fn extended_lengths_and_unaligned_tables_preserve_original_frame_addresses() {
    let mut table = vec![0];
    for id in [0_u64, 24] {
        table.extend(u32::MAX.to_le_bytes());
        table.extend(8_u64.to_le_bytes());
        table.extend(id.to_le_bytes());
    }
    table.extend(0_u32.to_le_bytes());
    assert_eq!(visited(&table[1..]), [20]);
}

#[test]
fn jit_compilation_and_caught_errors_keep_the_vm_usable() {
    let lua = mlua::Lua::new_with(
        mlua::StdLib::ALL_SAFE | mlua::StdLib::JIT,
        mlua::LuaOptions::default(),
    )
    .unwrap();
    lua.load(
        r#"
        assert(jit.status())
        jit.opt.start("hotloop=1")
        local function step(n)
            if n == 400 then error("fixture error") end
            return n + 1
        end
        for n = 1, 400 do
            local ok, value = pcall(step, n)
            assert(ok == (n < 400))
            if ok then assert(value == n + 1) end
        end
        assert(require("jit.util").traceinfo(1))
        assert(step(1) == 2)
        "#,
    )
    .exec()
    .unwrap();
}
