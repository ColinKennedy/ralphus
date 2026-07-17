//! Embeds `windows_resource.rc` (FileDescription/ProductName = "Ralphus") into
//! `ralphus-daemon.exe` so Task Manager shows a friendly name instead of the
//! raw filename (RAL-99). A no-op when not building for Windows.

fn main() {
    embed_resource::compile("windows_resource.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
