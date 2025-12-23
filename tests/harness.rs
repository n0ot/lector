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
