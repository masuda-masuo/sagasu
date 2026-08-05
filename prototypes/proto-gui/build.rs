// We intentionally do NOT include ../build-common.rs here.
// tauri_build::build() handles its own Windows resource embedding, and
// build-common.rs's rc.exe resource embed would collide with it.
fn main() {
    tauri_build::build()
}
