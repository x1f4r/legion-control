//! Desktop entry point for the Legion native shell.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if legion_app_lib::is_version_flag(&args) {
        println!("{}", legion_app_lib::version_line());
        return;
    }
    legion_app_lib::run();
}
