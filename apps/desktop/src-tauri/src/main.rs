// The desktop shell is a GUI application in every profile: a console window next to the
// window is never wanted, and a debug build is what gets run day to day here.
#![windows_subsystem = "windows"]

fn main() {
    llm_gateway_desktop_lib::run()
}
