//! Desktop entry point for the Legion native shell.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    legion_app_lib::run();
}
