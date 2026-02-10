use lector::harness::Harness;

#[test]
fn harness_script_basic() {
    let script = include_str!("scripts/basic.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_lua_repl() {
    let script = include_str!("scripts/lua_repl.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_table_headers() {
    let script = include_str!("scripts/table_headers.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_focus_events() {
    let script = include_str!("scripts/focus_events.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}
